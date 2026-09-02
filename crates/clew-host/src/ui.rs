use crate::outfit::{
    KEY_AWAITING_ENROLLMENT, KEY_CHOOSE_INVITE, KEY_EXIT_AND_DISCONNECT, KEY_EXTRACT_FIRST,
    KEY_HELPER_READY, KEY_HIDE_TO_TRAY, KEY_MISSING_INVITE_BODY, KEY_MISSING_INVITE_TITLE,
    KEY_READY, KEY_TRAY_EXIT, KEY_TRAY_SHOW, OutfitPreset, OutfitProfile,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiResources {
    pub app_name: String,
    pub window_title: String,
    pub ready: String,
    pub helper_ready: String,
    pub awaiting_enrollment: String,
    pub missing_invite_title: String,
    pub missing_invite_body: String,
    pub extract_first: String,
    pub choose_invite: String,
    pub hide_to_tray: String,
    pub exit_and_disconnect: String,
    pub tray_show: String,
    pub tray_exit: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutfitRuntimeView {
    pub outfit_id: String,
    pub revision: u32,
    pub primary_color: String,
    pub resources: UiResources,
}

impl OutfitRuntimeView {
    #[must_use]
    pub fn clew_original() -> Self {
        Self::from_profile(&OutfitProfile::preset(OutfitPreset::ClewOriginal), "en-US")
    }

    #[must_use]
    pub fn from_profile(profile: &OutfitProfile, locale: &str) -> Self {
        Self {
            outfit_id: profile.outfit_id.clone(),
            revision: profile.revision,
            primary_color: profile.visuals.primary_color.clone(),
            resources: UiResources {
                app_name: profile.identity.app_display_name.clone(),
                window_title: profile.identity.window_title.clone(),
                ready: profile.resolve_resource(locale, KEY_READY),
                helper_ready: profile.resolve_resource(locale, KEY_HELPER_READY),
                awaiting_enrollment: profile.resolve_resource(locale, KEY_AWAITING_ENROLLMENT),
                missing_invite_title: profile.resolve_resource(locale, KEY_MISSING_INVITE_TITLE),
                missing_invite_body: profile.resolve_resource(locale, KEY_MISSING_INVITE_BODY),
                extract_first: profile.resolve_resource(locale, KEY_EXTRACT_FIRST),
                choose_invite: profile.resolve_resource(locale, KEY_CHOOSE_INVITE),
                hide_to_tray: profile.resolve_resource(locale, KEY_HIDE_TO_TRAY),
                exit_and_disconnect: profile.resolve_resource(locale, KEY_EXIT_AND_DISCONNECT),
                tray_show: profile.resolve_resource(locale, KEY_TRAY_SHOW),
                tray_exit: profile.resolve_resource(locale, KEY_TRAY_EXIT),
            },
        }
    }
}

impl Default for OutfitRuntimeView {
    fn default() -> Self {
        Self::clew_original()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clew_original_contains_fixed_recovery_copy() {
        let view = OutfitRuntimeView::clew_original();
        assert_eq!(view.outfit_id, "clew-original");
        assert!(view.resources.missing_invite_body.contains("site.clew"));
        assert!(
            view.resources
                .extract_first
                .contains("Extract the complete archive")
        );
        assert!(view.resources.ready.is_ascii());
        assert!(view.resources.helper_ready.is_ascii());
        assert!(view.resources.awaiting_enrollment.is_ascii());
        assert!(view.resources.missing_invite_title.is_ascii());
        assert!(view.resources.missing_invite_body.is_ascii());
        assert!(view.resources.extract_first.is_ascii());
        assert!(view.resources.choose_invite.is_ascii());
        assert!(view.resources.hide_to_tray.is_ascii());
        assert!(view.resources.exit_and_disconnect.is_ascii());
        assert!(view.resources.tray_show.is_ascii());
        assert!(view.resources.tray_exit.is_ascii());
    }
}
