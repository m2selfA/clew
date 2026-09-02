#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiResources {
    pub app_name: &'static str,
    pub window_title: &'static str,
    pub ready: &'static str,
    pub awaiting_enrollment: &'static str,
    pub missing_invite_title: &'static str,
    pub missing_invite_body: &'static str,
    pub extract_first: &'static str,
    pub choose_invite: &'static str,
    pub hide_to_tray: &'static str,
    pub exit_and_disconnect: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutfitRuntimeView {
    pub outfit_id: &'static str,
    pub revision: u32,
    pub resources: UiResources,
}

impl OutfitRuntimeView {
    #[must_use]
    pub const fn clew_original() -> Self {
        Self {
            outfit_id: "clew-original",
            revision: 1,
            resources: UiResources {
                app_name: "Clew",
                window_title: "Clew",
                ready: "This computer is ready.",
                awaiting_enrollment: "Invitation verified. Connecting to the controller.",
                missing_invite_title: "An invitation file is still needed.",
                missing_invite_body: "Keep site.clew next to this app, or drop site.clew here.",
                extract_first: "Extract the complete archive before opening the app.",
                choose_invite: "Choose invitation file",
                hide_to_tray: "Hide to tray",
                exit_and_disconnect: "Exit and disconnect",
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
        assert!(view.resources.extract_first.contains("Extract the complete archive"));
    }
}
