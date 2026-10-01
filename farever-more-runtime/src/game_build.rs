//! Exact-build fast path and bytecode-verified compatibility for newer builds.

use crate::hashlink::{verify_build, BuildFileSpec, HashLinkBuildSpec};
use std::path::Path;

/// Final `GameApp.loadingState` shared by the exact supported builds.
pub(crate) const GAME_APP_LOADING_STATE_PLAYABLE: i32 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GameBuildProfile {
    Stable25257040,
    Beta25531577,
    Stable25628371,
    Stable25632706,
    Stable25658350,
    Compatible25632706,
}

impl GameBuildProfile {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Stable25257040 => "steam-25257040",
            Self::Beta25531577 => "steam-25531577-beta",
            Self::Stable25628371 => "steam-25628371",
            Self::Stable25632706 => "steam-25632706",
            Self::Stable25658350 => "steam-25658350",
            Self::Compatible25632706 => "bytecode-compatible-with-steam-25632706",
        }
    }

    /// ABI introduced by beta 25531577 and retained by stable 25628371.
    pub(crate) const fn uses_beta_abi(self) -> bool {
        matches!(
            self,
            Self::Beta25531577
                | Self::Stable25628371
                | Self::Stable25632706
                | Self::Stable25658350
                | Self::Compatible25632706
        )
    }

    /// `DamageResult` inherits this member from `BaseSkillAccess`. The beta
    /// renamed it while keeping the concrete type and damage-hook ABI stable.
    pub(crate) const fn base_skill_access_field(self) -> &'static str {
        match self {
            Self::Stable25257040 => "baseSkill",
            Self::Beta25531577
            | Self::Stable25628371
            | Self::Stable25632706
            | Self::Stable25658350
            | Self::Compatible25632706 => "skill",
        }
    }
}

const STABLE_FILES: &[BuildFileSpec] = &[
    BuildFileSpec {
        name: "Farever.exe",
        sha256: "96F5DFEEF6F1D3E1AA0810BE333CF23965E42C0C0328CFDBD02F2693913C28EB",
    },
    BuildFileSpec {
        name: "hlboot.dat",
        sha256: "7D0C189415AD6832B11DA0FAFDA29EAAA2A41F93330C4489942820B495E1DF89",
    },
    BuildFileSpec {
        name: "libhl.dll",
        sha256: "0D67CA75C73F93306158D67BD2B61763C539F3155375403CD52D8F16DB7F73ED",
    },
];

const BETA_FILES: &[BuildFileSpec] = &[
    BuildFileSpec {
        name: "Farever.exe",
        sha256: "186440648F9906C2E64955F9A7B2508D9C6842F90ABEE749187191A822CAEF36",
    },
    BuildFileSpec {
        name: "hlboot.dat",
        sha256: "42A7EE2E85ED9166510BDC7DF2BFDDB8ECEBCD10917A3FDEA94A8BC4F381E5ED",
    },
    BuildFileSpec {
        name: "libhl.dll",
        sha256: "0F6FB5D60D42039BE36D1426098A73127388A892AB1E59D2C81DD33AC9C279FB",
    },
];

const STABLE: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "steam-25257040",
    files: STABLE_FILES,
};
const BETA: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "steam-25531577-beta",
    files: BETA_FILES,
};

// Reviewed against the 25531577 full metadata inventory. Gameplay offsets
// remain resolved by name and every hook still validates its live signature.
const STABLE_25628371: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "steam-25628371",
    files: &[
        BETA_FILES[0],
        BuildFileSpec {
            name: "hlboot.dat",
            sha256: "8BB2CE5180018EBFFE0C3F357C77B86BF5EBCECD06AA65B97A69367F2F806E49",
        },
        BETA_FILES[2],
    ],
};

// The full semantic diff changes NPC icon records and unhooked methods; the
// host's required signatures, schemas and final loading-state pattern match.
const STABLE_25632706: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "steam-25632706",
    files: &[
        BETA_FILES[0],
        BuildFileSpec {
            name: "hlboot.dat",
            sha256: "D48F5F511A8257832EF470FCBA82A0A7F5DB402CE8EABD11CAF42BBBA21E4280",
        },
        BETA_FILES[2],
    ],
};

const CURRENT_NATIVE: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "reviewed-native-runtime-25632706",
    files: &[BETA_FILES[0], BETA_FILES[2]],
};

// Full metadata review and the embedded contract retain the host ABI. Player,
// Hero and map schemas are unchanged; live startup remains independently gated.
const STABLE_25658350: HashLinkBuildSpec = HashLinkBuildSpec {
    profile: "steam-25658350",
    files: &[
        BETA_FILES[0],
        BuildFileSpec {
            name: "hlboot.dat",
            sha256: "617F602066C762869D5C15A01C83714664B0026868F0D24B129BBCC850DA88D8",
        },
        BETA_FILES[2],
    ],
};

pub(crate) fn verify_installed(directory: &Path) -> Result<(GameBuildProfile, String), String> {
    verify_with(directory, |path| {
        let contract: farever_api_inspector::CompatibilityContract =
            serde_json::from_str(include_str!("../assets/game-compatibility-25632706.json"))
                .map_err(|error| format!("invalid embedded compatibility contract: {error}"))?;
        if contract.baseline_sha256 != STABLE_25632706.files[1].sha256 {
            return Err("compatibility contract does not match its reviewed baseline".to_owned());
        }
        farever_api_inspector::verify_bytecode_contract(
            path,
            &contract,
            GAME_APP_LOADING_STATE_PLAYABLE,
        )
    })
}

fn verify_with(
    directory: &Path,
    check_bytecode: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(GameBuildProfile, String), String> {
    verify_profiles(
        directory,
        &[
            (GameBuildProfile::Stable25658350, &STABLE_25658350),
            (GameBuildProfile::Stable25632706, &STABLE_25632706),
            (GameBuildProfile::Stable25628371, &STABLE_25628371),
            (GameBuildProfile::Stable25257040, &STABLE),
            (GameBuildProfile::Beta25531577, &BETA),
        ],
        &CURRENT_NATIVE,
        check_bytecode,
    )
}

fn verify_profiles(
    directory: &Path,
    profiles: &[(GameBuildProfile, &HashLinkBuildSpec)],
    native: &HashLinkBuildSpec,
    check_bytecode: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(GameBuildProfile, String), String> {
    let mut errors = Vec::new();
    for &(profile, spec) in profiles {
        match verify_build(directory, spec) {
            Ok(hashes) => return Ok((profile, format!("verification=fingerprint {hashes}"))),
            Err(error) => errors.push(error),
        }
    }
    verify_build(directory, native).map_err(|error| format!(
        "unknown native executable or HashLink runtime; bytecode signatures cannot verify native ABI: {error}"
    ))?;
    check_bytecode(&directory.join("hlboot.dat")).map_err(|error| {
        format!(
            "game bytecode compatibility check failed: {error}; {}",
            errors[0]
        )
    })?;
    let prefix = format!("unknown game build profile={} ", profiles[0].1.profile);
    let hashes = errors[0].strip_prefix(&prefix).unwrap_or(&errors[0]);
    Ok((
        GameBuildProfile::Compatible25632706,
        format!(
            "verification=bytecode-signatures baseline=steam-25632706 {}",
            hashes
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn damage_result_skill_member_tracks_the_exact_game_profile() {
        assert_eq!(
            GameBuildProfile::Stable25257040.base_skill_access_field(),
            "baseSkill"
        );
        assert_eq!(
            GameBuildProfile::Beta25531577.base_skill_access_field(),
            "skill"
        );
        assert_eq!(
            GameBuildProfile::Stable25628371.base_skill_access_field(),
            "skill"
        );
        assert!(GameBuildProfile::Stable25628371.uses_beta_abi());
        assert!(GameBuildProfile::Beta25531577.uses_beta_abi());
        assert!(GameBuildProfile::Stable25632706.uses_beta_abi());
        assert!(GameBuildProfile::Stable25658350.uses_beta_abi());
        assert_eq!(
            GameBuildProfile::Stable25658350.base_skill_access_field(),
            "skill"
        );
        assert_eq!(
            GameBuildProfile::Stable25632706.base_skill_access_field(),
            "skill"
        );
        assert!(!GameBuildProfile::Stable25257040.uses_beta_abi());
        assert!(GameBuildProfile::Compatible25632706.uses_beta_abi());
        assert_eq!(
            GameBuildProfile::Compatible25632706.base_skill_access_field(),
            "skill"
        );
    }

    #[test]
    fn fingerprints_bypass_scan_unknown_bytecode_scans_and_native_changes_refuse() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        const EMPTY: &str = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        const FILES: &[BuildFileSpec] = &[
            BuildFileSpec {
                name: "Farever.exe",
                sha256: EMPTY,
            },
            BuildFileSpec {
                name: "hlboot.dat",
                sha256: EMPTY,
            },
            BuildFileSpec {
                name: "libhl.dll",
                sha256: EMPTY,
            },
        ];
        let dir = std::env::temp_dir().join(format!(
            "farever-compatibility-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for file in FILES {
            std::fs::write(dir.join(file.name), []).unwrap();
        }
        let spec = HashLinkBuildSpec {
            profile: "fixture",
            files: FILES,
        };
        let profiles = [(GameBuildProfile::Stable25628371, &spec)];
        const NATIVE_FILES: &[BuildFileSpec] = &[FILES[0], FILES[2]];
        let native = HashLinkBuildSpec {
            profile: "native",
            files: NATIVE_FILES,
        };
        assert_eq!(
            verify_profiles(&dir, &profiles, &native, |_| panic!(
                "known build must bypass scan"
            ))
            .unwrap()
            .0,
            GameBuildProfile::Stable25628371
        );
        std::fs::write(dir.join("hlboot.dat"), b"new version").unwrap();
        assert_eq!(
            verify_profiles(&dir, &profiles, &native, |path| {
                assert_eq!(path.file_name().unwrap(), "hlboot.dat");
                Ok(())
            })
            .unwrap()
            .0,
            GameBuildProfile::Compatible25632706
        );
        assert!(verify_profiles(&dir, &profiles, &native, |_| Err(
            "changed hook signature".into()
        ))
        .unwrap_err()
        .contains("changed hook signature"));
        std::fs::write(dir.join("libhl.dll"), b"new native runtime").unwrap();
        assert!(verify_profiles(&dir, &profiles, &native, |_| panic!(
            "unknown native runtime must refuse before scan"
        ))
        .unwrap_err()
        .contains("native ABI"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Run explicitly against the installed game's bytecode while the game is
    /// closed. Reviewed fingerprints and compatible newer bytecode both qualify.
    #[test]
    #[ignore = "requires FAREVER_GAME_DIRECTORY pointing to the reviewed installation"]
    fn reviewed_installation_satisfies_embedded_bytecode_contract() {
        let Some(directory) = std::env::var_os("FAREVER_GAME_DIRECTORY") else {
            // CI runs component smoke tests without an installed game.
            eprintln!("Skipping installed-game QA: set FAREVER_GAME_DIRECTORY to run it.");
            return;
        };
        let contract =
            serde_json::from_str(include_str!("../assets/game-compatibility-25632706.json"))
                .unwrap();
        farever_api_inspector::verify_bytecode_contract(
            &Path::new(&directory).join("hlboot.dat"),
            &contract,
            GAME_APP_LOADING_STATE_PLAYABLE,
        )
        .unwrap();
        let directory = Path::new(&directory);
        let (profile, details) = verify_installed(directory).unwrap();
        assert!(profile.uses_beta_abi());
        if profile == GameBuildProfile::Compatible25632706 {
            assert!(details.contains("verification=bytecode-signatures"));
        } else {
            assert!(details.contains("verification=fingerprint"));
        }
        eprintln!("Installed game profile={} {details}", profile.name());
        // Only offer the old native pair as a fingerprint. It cannot match
        // CURRENT_NATIVE, so this always exercises the real bytecode fallback.
        let (profile, details) = verify_profiles(
            directory,
            &[(GameBuildProfile::Stable25257040, &STABLE)],
            &CURRENT_NATIVE,
            |path| {
                farever_api_inspector::verify_bytecode_contract(
                    path,
                    &contract,
                    GAME_APP_LOADING_STATE_PLAYABLE,
                )
            },
        )
        .unwrap();
        assert_eq!(profile, GameBuildProfile::Compatible25632706);
        assert!(details.contains("verification=bytecode-signatures"));
    }
}
