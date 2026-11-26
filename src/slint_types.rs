//! Slint bindings and conversions to application types.

slint::include_modules!();

impl From<crate::player::state::LoopMode> for LoopMode {
    fn from(mode: crate::player::state::LoopMode) -> Self {
        use crate::player::state::LoopMode as Mode;
        match mode {
            Mode::None => Self::None,
            Mode::Track => Self::Track,
            Mode::Playlist => Self::Playlist,
        }
    }
}

impl From<LoopMode> for crate::player::state::LoopMode {
    fn from(mode: LoopMode) -> Self {
        match mode {
            LoopMode::None => Self::None,
            LoopMode::Track => Self::Track,
            LoopMode::Playlist => Self::Playlist,
        }
    }
}

impl From<crate::library::SortKey> for SortKey {
    fn from(key: crate::library::SortKey) -> Self {
        use crate::library::SortKey as Key;
        match key {
            Key::Title => Self::BySongName,
            Key::Artist => Self::BySinger,
            Key::Duration => Self::ByDuration,
        }
    }
}

impl From<SortKey> for crate::library::SortKey {
    fn from(key: SortKey) -> Self {
        match key {
            SortKey::BySongName => Self::Title,
            SortKey::BySinger => Self::Artist,
            SortKey::ByDuration => Self::Duration,
        }
    }
}
