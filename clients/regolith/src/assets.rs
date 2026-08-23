//! Optional visual assets with primitive fallback.

use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use std::path::{Path, PathBuf};

/// Paths for swappable visual-only glTF scenes.
#[derive(Debug, Clone, Resource)]
pub struct VisualAssetPaths {
    root: PathBuf,
    /// Craft scene relative to the asset root.
    pub craft: PathBuf,
}

impl Default for VisualAssetPaths {
    fn default() -> Self {
        Self {
            root: PathBuf::from("assets"),
            craft: PathBuf::from("regolith/craft.glb"),
        }
    }
}

impl VisualAssetPaths {
    /// Resolve the optional craft scene when it is present on disk.
    ///
    /// This result is presentation-only. Asset geometry must never feed
    /// collision, hitboxes, reach, or any other simulation input: those shapes
    /// belong to `orrery_games::regolith`, so replacing art cannot move a
    /// determinism golden or split bot hours from human hours.
    #[must_use]
    pub fn craft_scene(&self, asset_server: &AssetServer) -> Option<Handle<WorldAsset>> {
        if !self.root.join(&self.craft).is_file() {
            return None;
        }
        let asset_path = self.craft.to_string_lossy().replace('\\', "/");
        Some(asset_server.load(GltfAssetLabel::Scene(0).from_asset(asset_path)))
    }

    /// Root searched before an asset is requested.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}
