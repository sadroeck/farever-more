//! Exact game-file fingerprints and ABI choices for supported Steam builds.

use crate::hashlink::{verify_build, BuildFileSpec, HashLinkBuildSpec};
use std::path::Path;

/// Final `GameApp.loadingState` shared by the two exact supported builds.
pub(crate) const GAME_APP_LOADING_STATE_PLAYABLE: i32 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GameBuildProfile {
    Stable25257040,
    Beta25531577,
}

impl GameBuildProfile {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Stable25257040 => "steam-25257040",
            Self::Beta25531577 => "steam-25531577-beta",
        }
    }

    pub(crate) const fn is_beta(self) -> bool {
        matches!(self, Self::Beta25531577)
    }

    /// `DamageResult` inherits this member from `BaseSkillAccess`. The beta
    /// renamed it while keeping the concrete type and damage-hook ABI stable.
    pub(crate) const fn base_skill_access_field(self) -> &'static str {
        match self {
            Self::Stable25257040 => "baseSkill",
            Self::Beta25531577 => "skill",
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

pub(crate) fn verify_installed(directory: &Path) -> Result<(GameBuildProfile, String), String> {
    let mut errors = Vec::new();
    for (profile, spec) in [
        (GameBuildProfile::Stable25257040, &STABLE),
        (GameBuildProfile::Beta25531577, &BETA),
    ] {
        match verify_build(directory, spec) {
            Ok(hashes) => return Ok((profile, hashes)),
            Err(error) => errors.push(error),
        }
    }
    Err(format!(
        "no supported exact game profile matched: {}",
        errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::GameBuildProfile;

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
    }
}
