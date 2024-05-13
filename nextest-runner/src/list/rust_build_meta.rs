// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    errors::RustBuildMetaParseError,
    helpers::convert_rel_path_to_main_sep,
    list::{BinaryListState, TestListState},
    platform::{BuildPlatforms, FromSummary, ToSummary},
    reuse_build::PathMapper,
};
use camino::Utf8PathBuf;
use itertools::Itertools;
use nextest_metadata::{BuildPlatformsSummary, RustBuildMetaSummary, RustNonTestBinarySummary};
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
};

/// Rust-related metadata used for builds and test runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustBuildMeta<State> {
    /// The target directory for build artifacts.
    pub target_directory: Utf8PathBuf,

    /// A list of base output directories, relative to the target directory. These directories
    /// and their "deps" subdirectories are added to the dynamic library path.
    pub base_output_directories: BTreeSet<Utf8PathBuf>,

    /// Information about non-test executables, keyed by package ID.
    pub non_test_binaries: BTreeMap<String, BTreeSet<RustNonTestBinarySummary>>,

    /// Build script output directory, relative to the target directory and keyed by package ID.
    /// Only present for workspace packages that have build scripts.
    pub build_script_out_dirs: BTreeMap<String, Utf8PathBuf>,

    /// A list of linked paths, relative to the target directory. These directories are
    /// added to the dynamic library path.
    ///
    /// The values are the package IDs of the libraries that requested the linked paths.
    ///
    /// Note that the serialized metadata only has the paths for now, not the libraries that
    /// requested them. We might consider adding a new field with metadata about that.
    pub linked_paths: BTreeMap<Utf8PathBuf, BTreeSet<String>>,

    /// The build platforms: host and target triple
    pub build_platforms: BuildPlatforms,

    state: PhantomData<State>,
}

impl RustBuildMeta<BinaryListState> {
    /// Creates a new [`RustBuildMeta`].
    pub fn new(target_directory: impl Into<Utf8PathBuf>, build_platforms: BuildPlatforms) -> Self {
        Self {
            target_directory: target_directory.into(),
            base_output_directories: BTreeSet::new(),
            non_test_binaries: BTreeMap::new(),
            build_script_out_dirs: BTreeMap::new(),
            linked_paths: BTreeMap::new(),
            state: PhantomData,
            build_platforms,
        }
    }

    /// Maps paths using a [`PathMapper`] to convert this to [`TestListState`].
    pub fn map_paths(&self, path_mapper: &PathMapper) -> RustBuildMeta<TestListState> {
        RustBuildMeta {
            target_directory: path_mapper
                .new_target_dir()
                .unwrap_or(&self.target_directory)
                .to_path_buf(),
            // Since these are relative paths, they don't need to be mapped.
            base_output_directories: self.base_output_directories.clone(),
            non_test_binaries: self.non_test_binaries.clone(),
            build_script_out_dirs: self.build_script_out_dirs.clone(),
            linked_paths: self.linked_paths.clone(),
            state: PhantomData,
            build_platforms: self.build_platforms.clone(),
        }
    }
}

impl RustBuildMeta<TestListState> {
    /// Empty metadata for tests.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            target_directory: Utf8PathBuf::new(),
            base_output_directories: BTreeSet::new(),
            non_test_binaries: BTreeMap::new(),
            build_script_out_dirs: BTreeMap::new(),
            linked_paths: BTreeMap::new(),
            state: PhantomData,
            build_platforms: BuildPlatforms::new().unwrap(),
        }
    }

    /// Returns the dynamic library paths corresponding to this metadata.
    ///
    /// [See this Cargo documentation for more.](https://doc.rust-lang.org/cargo/reference/environment-variables.html#dynamic-library-paths)
    ///
    /// These paths are prepended to the dynamic library environment variable for the current
    /// platform (e.g. `LD_LIBRARY_PATH` on non-Apple Unix platforms).
    pub fn dylib_paths(&self) -> Vec<Utf8PathBuf> {
        // FIXME/HELP WANTED: get the rustc sysroot library path here.
        // See https://github.com/nextest-rs/nextest/issues/267.

        let libdirs = self
            .build_platforms
            .host_libdir
            .iter()
            .chain(
                self.build_platforms
                    .target
                    .as_ref()
                    .and_then(|target| target.libdir.as_ref()),
            )
            .cloned()
            .collect::<Vec<_>>();
        if libdirs.is_empty() {
            log::warn!("failed to detect the rustc libdir, may fail to list or run tests");
        }
        // Cargo puts linked paths before base output directories.
        self.linked_paths
            .keys()
            .filter_map(|rel_path| {
                let join_path = self
                    .target_directory
                    .join(convert_rel_path_to_main_sep(rel_path));
                // Only add the directory to the path if it exists on disk.
                join_path.exists().then_some(join_path)
            })
            .chain(self.base_output_directories.iter().flat_map(|base_output| {
                let abs_base = self
                    .target_directory
                    .join(convert_rel_path_to_main_sep(base_output));
                let with_deps = abs_base.join("deps");
                // This is the order paths are added in by Cargo.
                [with_deps, abs_base]
            }))
            // Add the rustc libdir paths to the search paths to run procudure macro binaries. See
            // details in https://github.com/nextest-rs/nextest/issues/1493.
            .chain(libdirs)
            .unique()
            .collect()
    }
}

impl<State> RustBuildMeta<State> {
    /// Creates a `RustBuildMeta` from a serializable summary.
    pub fn from_summary(summary: RustBuildMetaSummary) -> Result<Self, RustBuildMetaParseError> {
        let build_platforms = if let Some(summary) = summary.platforms {
            BuildPlatforms::from_summary(summary.clone())?
        } else if let Some(summary) = summary.target_platforms.first() {
            // Compatibility with metadata generated by older versions of nextest.
            BuildPlatforms::from_summary(summary.clone())?
        } else {
            // Compatibility with metadata generated by older versions of nextest.
            BuildPlatforms::from_summary_str(summary.target_platform.clone())?
        };

        Ok(Self {
            target_directory: summary.target_directory,
            base_output_directories: summary.base_output_directories,
            build_script_out_dirs: summary.build_script_out_dirs,
            non_test_binaries: summary.non_test_binaries,
            linked_paths: summary
                .linked_paths
                .into_iter()
                .map(|linked_path| (linked_path, BTreeSet::new()))
                .collect(),
            state: PhantomData,
            build_platforms,
        })
    }

    /// Converts self to a serializable form.
    pub fn to_summary(&self) -> RustBuildMetaSummary {
        RustBuildMetaSummary {
            target_directory: self.target_directory.clone(),
            base_output_directories: self.base_output_directories.clone(),
            non_test_binaries: self.non_test_binaries.clone(),
            build_script_out_dirs: self.build_script_out_dirs.clone(),
            linked_paths: self.linked_paths.keys().cloned().collect(),
            target_platform: self.build_platforms.to_summary_str(),
            target_platforms: vec![self.build_platforms.to_summary()],
            // TODO: support multiple --target options
            platforms: Some(BuildPlatformsSummary {
                host: self.build_platforms.to_summary(),
                targets: self
                    .build_platforms
                    .target
                    .as_ref()
                    .into_iter()
                    .map(ToSummary::to_summary)
                    .collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cargo_config::TargetTriple,
        platform::{BuildPlatforms, BuildPlatformsTarget},
    };
    use nextest_metadata::{BuildPlatformsSummary, HostPlatformSummary, TargetPlatformSummary};
    use target_spec::{summaries::PlatformSummary, Platform};
    use test_case::test_case;

    impl Default for RustBuildMeta<BinaryListState> {
        fn default() -> Self {
            RustBuildMeta::<BinaryListState>::new(
                Utf8PathBuf::default(),
                BuildPlatforms::new()
                    .expect("creating BuildPlatforms without target triple should succeed"),
            )
        }
    }

    fn x86_64_pc_windows_msvc_triple() -> TargetTriple {
        TargetTriple::deserialize_str(Some("x86_64-pc-windows-msvc".to_owned()))
            .expect("creating TargetTriple from windows msvc triple string should succeed")
            .expect("the output of deserialize_str shouldn't be None")
    }

    fn x86_64_apple_darwin_triple() -> TargetTriple {
        TargetTriple::deserialize_str(Some("x86_64-apple-darwin".to_owned()))
            .expect("creating TargetTriple from apple triple string should succeed")
            .expect("the output of deserialize_str shouldn't be None")
    }

    fn aarch64_unknown_linux_gnu_triple() -> TargetTriple {
        TargetTriple::deserialize_str(Some("aarch64-unknown-linux-gnu".to_owned()))
            .expect("creating TargetTriple from ARM Linux triple string should succeed")
            .expect("the output of deserialize_str shouldn't be None")
    }

    fn host_platform() -> Platform {
        Platform::current().expect("should detect the host platform successfully")
    }

    #[test_case(RustBuildMetaSummary {
        ..Default::default()
    }, RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: host_platform(),
            target: None,
            host_libdir: None,
        },
        ..Default::default()
    }; "no target platforms")]
    #[test_case(RustBuildMetaSummary {
        target_platform: Some("x86_64-unknown-linux-gnu".to_owned()),
        ..Default::default()
    }, RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: host_platform(),
            host_libdir: None,
            target: Some(BuildPlatformsTarget{
                triple: TargetTriple::x86_64_unknown_linux_gnu(),
                libdir: None,
            }),
        },
        ..Default::default()
    }; "only target platform field")]
    #[test_case(RustBuildMetaSummary {
        target_platform: Some("x86_64-unknown-linux-gnu".to_owned()),
        target_platforms: vec![PlatformSummary::new("x86_64-pc-windows-msvc")],
        ..Default::default()
    }, RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: host_platform(),
            host_libdir: None,
            target: Some(BuildPlatformsTarget{
                triple: x86_64_pc_windows_msvc_triple(),
                libdir: None,
            }),
        },
        ..Default::default()
    }; "target platform and target platforms field")]
    #[test_case(RustBuildMetaSummary {
        target_platform: Some("x86_64-unknown-linux-gnu".to_owned()),
        target_platforms: vec![PlatformSummary::new("x86_64-pc-windows-msvc")],
        platforms: Some(BuildPlatformsSummary {
            host: HostPlatformSummary {
                platform: not_host_platform_triple().platform.to_summary(),
                libdir: Some("/fake/test/libdir/281".into()),
            },
            targets: vec![TargetPlatformSummary {
                platform: PlatformSummary::new("aarch64-unknown-linux-gnu"),
                libdir: Some("/fake/test/libdir/837".into()),
            }],
        }),
        ..Default::default()
    }, RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: not_host_platform_triple().platform,
            host_libdir: Some("/fake/test/libdir/281".into()),
            target: Some(BuildPlatformsTarget{
                triple: aarch64_unknown_linux_gnu_triple(),
                libdir: Some("/fake/test/libdir/837".into()),
            }),
        },
        ..Default::default()
    }; "target platform and target platforms and platforms field")]
    #[test_case(RustBuildMetaSummary {
        platforms: Some(BuildPlatformsSummary {
            host: HostPlatformSummary {
                platform: PlatformSummary::new("x86_64-apple-darwin"),
                libdir: None,
            },
            targets: vec![],
        }),
        ..Default::default()
    }, RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: x86_64_apple_darwin_triple().platform,
            host_libdir: None,
            target: None,
        },
        ..Default::default()
    }; "platforms with zero targets")]
    fn test_from_summary(summary: RustBuildMetaSummary, expected: RustBuildMeta<BinaryListState>) {
        let actual = RustBuildMeta::<BinaryListState>::from_summary(summary)
            .expect("RustBuildMeta should deserialize from summary with success.");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_from_summary_error_multiple_targets() {
        let summary = RustBuildMetaSummary {
            platforms: Some(BuildPlatformsSummary {
                host: HostPlatformSummary {
                    platform: PlatformSummary::new("x86_64-apple-darwin"),
                    libdir: None,
                },
                targets: vec![
                    TargetPlatformSummary {
                        platform: PlatformSummary::new("aarch64-unknown-linux-gnu"),
                        libdir: None,
                    },
                    TargetPlatformSummary {
                        platform: PlatformSummary::new("x86_64-pc-windows-msvc"),
                        libdir: None,
                    },
                ],
            }),
            ..Default::default()
        };
        let actual = RustBuildMeta::<BinaryListState>::from_summary(summary);
        assert!(matches!(actual, Err(RustBuildMetaParseError::Unsupported { .. })), "Expect the parse result to be an error of RustBuildMetaParseError::Unsupported, actual {:?}", actual);
    }

    #[test]
    fn test_from_summary_error_invalid_host_platform_summary() {
        let summary = RustBuildMetaSummary {
            platforms: Some(BuildPlatformsSummary {
                host: HostPlatformSummary {
                    platform: PlatformSummary::new("invalid-platform-triple"),
                    libdir: None,
                },
                targets: vec![],
            }),
            ..Default::default()
        };
        let actual = RustBuildMeta::<BinaryListState>::from_summary(summary);
        assert!(
            actual.is_err(),
            "Expect the parse result to be an error, actual {:?}",
            actual
        );
    }

    fn not_host_platform_triple() -> TargetTriple {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                TargetTriple::x86_64_unknown_linux_gnu()
            } else {
                x86_64_pc_windows_msvc_triple()
            }
        }
    }

    #[test_case(RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: host_platform(),
            target: None,
            host_libdir: None,
        },
        ..Default::default()
    }, RustBuildMetaSummary {
        target_platform: None,
        target_platforms: vec![host_platform().to_summary()],
        platforms: Some(BuildPlatformsSummary {
            host: HostPlatformSummary {
                platform: host_platform().to_summary(),
                libdir: None,
            },
            targets: vec![],
        }),
        ..Default::default()
    }; "build platforms without target")]
    #[test_case(RustBuildMeta::<BinaryListState> {
        build_platforms: BuildPlatforms {
            host: host_platform(),
            host_libdir: Some("/fake/test/libdir/736".into()),
            target: Some(BuildPlatformsTarget {
                triple: not_host_platform_triple(),
                libdir: Some(Utf8PathBuf::from("/fake/test/libdir/873")),
            }),
        },
        ..Default::default()
    }, RustBuildMetaSummary {
        target_platform: Some(not_host_platform_triple().platform.triple_str().to_owned()),
        target_platforms: vec![not_host_platform_triple().platform.to_summary()],
        platforms: Some(BuildPlatformsSummary {
            host: HostPlatformSummary {
                platform: host_platform().to_summary(),
                libdir: Some("/fake/test/libdir/736".into()),
            },
            targets: vec![TargetPlatformSummary {
                platform: not_host_platform_triple().platform.to_summary(),
                libdir: Some("/fake/test/libdir/873".into()),
            }],
        }),
        ..Default::default()
    }; "build platforms with target")]
    fn test_to_summary(meta: RustBuildMeta<BinaryListState>, expected: RustBuildMetaSummary) {
        let actual = meta.to_summary();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dylib_paths_should_include_rustc_dir() {
        let host_libdir = Utf8PathBuf::from("/fake/rustc/host/libdir");
        let target_libdir = Utf8PathBuf::from("/fake/rustc/target/libdir");

        let rust_build_meta = RustBuildMeta {
            build_platforms: {
                let mut build_platforms = BuildPlatforms::new()
                    .expect("Should create BuildPlatforms with default ctor successfully");
                build_platforms.host_libdir = Some(host_libdir.clone());
                let mut target =
                    BuildPlatformsTarget::new(TargetTriple::x86_64_unknown_linux_gnu());
                target.libdir = Some(target_libdir.clone());
                build_platforms.target = Some(target);
                build_platforms
            },
            ..RustBuildMeta::empty()
        };
        let dylib_paths = rust_build_meta.dylib_paths();

        assert!(
            dylib_paths.contains(&host_libdir),
            "{:?} should contain {}",
            dylib_paths,
            host_libdir
        );
        assert!(
            dylib_paths.contains(&target_libdir),
            "{:?} should contain {}",
            dylib_paths,
            target_libdir
        );
    }

    #[test]
    fn test_dylib_paths_should_not_contain_duplicate_paths() {
        let tmpdir = camino_tempfile::tempdir().expect("should create temp dir successfully");
        let host_libdir = tmpdir.path().to_path_buf();
        let target_libdir = host_libdir.clone();
        let fake_target_dir = tmpdir
            .path()
            .parent()
            .expect("tmp directory should have a parent");
        let tmpdir_dirname = tmpdir
            .path()
            .file_name()
            .expect("tmp directory should have a file name");

        let rust_build_meta = RustBuildMeta {
            target_directory: fake_target_dir.to_path_buf(),
            linked_paths: [(Utf8PathBuf::from(tmpdir_dirname), Default::default())].into(),
            base_output_directories: [Utf8PathBuf::from(tmpdir_dirname)].into(),
            build_platforms: {
                let mut build_platforms = BuildPlatforms::new()
                    .expect("should create BuildPlatforms with default ctor successfully");
                let mut target =
                    BuildPlatformsTarget::new(TargetTriple::x86_64_unknown_linux_gnu());
                target.libdir = Some(target_libdir.clone());
                build_platforms.target = Some(target);
                build_platforms
            },
            ..RustBuildMeta::empty()
        };
        let dylib_paths = rust_build_meta.dylib_paths();

        assert!(
            dylib_paths.clone().into_iter().all_unique(),
            "{:?} should not contain duplicate paths",
            dylib_paths
        );
    }
}
