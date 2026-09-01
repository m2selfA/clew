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
                ready: "这台电脑已准备好。",
                awaiting_enrollment: "邀请已验证，正在等待连接 Controller。",
                missing_invite_title: "还缺一个邀请文件。",
                missing_invite_body: "请把 site.clew 和这个程序放在同一个文件夹，或把 site.clew 拖到这里。",
                extract_first: "请先全部解压这个压缩包，再打开程序。",
                choose_invite: "选择邀请文件",
                hide_to_tray: "隐藏到托盘",
                exit_and_disconnect: "退出并断开",
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
        assert!(view.resources.extract_first.contains("全部解压"));
    }
}
