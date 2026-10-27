use super::*;

impl App {
    /// `/model`: open the model-picker overlay populated from the retained
    /// `model_choices`. The current model is pre-selected so the user sees
    /// where they are. No-op (with a notice) when no model is available.
    pub(in crate::tui) fn open_model_picker(&mut self) {
        if self.model_choices.is_empty() {
            self.notify(
                NotifyKind::Warn,
                "no models available (set a provider API key or configure backend models)",
            );
            return;
        }
        let selected = self
            .model_choices
            .iter()
            .position(|c| {
                format!("{}/{}", c.provider, c.id)
                    == self
                        .selected_model_label
                        .as_ref()
                        .unwrap_or(&self.model_label)
                        .as_str()
            })
            .unwrap_or(0);
        self.model_picker = Some(ModelPickerState {
            choices: self.model_choices.clone(),
            selected,
        });
    }

    pub(in crate::tui) fn model_picker_confirm(&mut self) {
        if let Some(picker) = self.model_picker.take() {
            if let Some(choice) = picker.choices.get(picker.selected) {
                self.pending_model_switch = Some(format!("{}/{}", choice.provider, choice.id));
            }
        }
    }

    /// `/thinking`: open the thinking-level picker for the current model.
    /// Offers `off` plus the model's declared `thinking_levels` (deduped),
    /// pre-selected at the current level. Shows a notice instead of opening
    /// when the current model supports no thinking levels (only `off`).
    pub(in crate::tui) fn open_thinking_picker(&mut self) {
        let levels = self.current_thinking_choices();
        if levels.len() <= 1 {
            self.notify(
                NotifyKind::Info,
                "current model does not support thinking levels",
            );
            return;
        }
        let selected = levels.iter().position(|l| l == &self.thinking).unwrap_or(0);
        self.thinking_picker = Some(ThinkingPickerState { levels, selected });
    }

    fn current_thinking_choices(&self) -> Vec<ThinkingLevel> {
        let mut out = vec![ThinkingLevel::Off];
        let label = self
            .selected_model_label
            .as_ref()
            .unwrap_or(&self.model_label);
        if let Some(c) = self
            .model_choices
            .iter()
            .find(|c| format!("{}/{}", c.provider, c.id) == label.as_str())
        {
            for level in &c.thinking_levels {
                if level != &ThinkingLevel::Off && !out.contains(level) {
                    out.push(level.clone());
                }
            }
        }
        out
    }

    /// `/service`: open the service-tier picker for the current model.
    /// Offers `auto` plus the model's declared `service_tiers` (deduped),
    /// pre-selected at the current tier. Shows a notice instead of opening
    /// when the current model declares no service tiers beyond `auto`.
    pub(in crate::tui) fn open_service_picker(&mut self) {
        let tiers = self.current_service_choices();
        if tiers.len() <= 1 {
            self.notify(
                NotifyKind::Info,
                "current model does not support service tiers",
            );
            return;
        }
        let selected = tiers
            .iter()
            .position(|t| t == &self.service_tier)
            .unwrap_or(0);
        self.service_picker = Some(ServicePickerState { tiers, selected });
    }

    fn current_service_choices(&self) -> Vec<ServiceTier> {
        let mut out = vec![ServiceTier::Auto];
        if let Some(c) = self
            .model_choices
            .iter()
            .find(|c| format!("{}/{}", c.provider, c.id) == self.model_label)
        {
            for tier in &c.service_tiers {
                if tier != &ServiceTier::Auto && !out.contains(tier) {
                    out.push(tier.clone());
                }
            }
        }
        out
    }

    pub(in crate::tui) fn service_picker_confirm(&mut self) {
        if let Some(picker) = self.service_picker.take() {
            if let Some(tier) = picker.tiers.get(picker.selected) {
                let tier_suffix = if tier == &ServiceTier::Auto {
                    String::new()
                } else {
                    format!("@{}", tier.as_str())
                };
                self.pending_model_switch = Some(format!(
                    "{}:{}{}",
                    self.switch_label(),
                    self.thinking.as_str(),
                    tier_suffix
                ));
            }
        }
    }

    /// `/theme`: open the color-scheme picker (Auto / Light / Dark),
    /// pre-selected on the currently active mode.
    pub(in crate::tui) fn open_theme_picker(&mut self) {
        let modes = ThemePickerState::MODES;
        let selected = modes
            .iter()
            .position(|m| *m == self.theme_manager.mode())
            .unwrap_or(0);
        self.theme_picker = Some(ThemePickerState { modes, selected });
    }

    pub(in crate::tui) fn theme_picker_confirm(&mut self) {
        if let Some(picker) = self.theme_picker.take() {
            if let Some(&mode) = picker.modes.get(picker.selected) {
                self.update_theme(theme::ThemeEvent::SetMode(mode));
            }
        }
    }

    /// `/policy`: open the bash approval mode dialog. The choice is
    /// session-scoped and never written to `policy.toml`.
    pub(in crate::tui) fn open_policy_picker(&mut self) {
        let Some(override_handle) = &self.policy_override else {
            self.notify(NotifyKind::Error, "no agent: /policy is unavailable");
            return;
        };
        let mut modes = vec![
            lofi_types::BashApprovalMode::AllowAll,
            lofi_types::BashApprovalMode::AskManual,
        ];
        if self.auto_mode_configured {
            modes.push(lofi_types::BashApprovalMode::AskAuto);
        }
        modes.push(lofi_types::BashApprovalMode::DenyAll);
        let current = override_handle.effective(self.auto_mode_configured);
        let selected = modes.iter().position(|m| *m == current).unwrap_or(1);
        self.policy_picker = Some(PolicyPickerState { modes, selected });
    }

    pub(in crate::tui) fn policy_picker_confirm(&mut self) {
        if let Some(picker) = self.policy_picker.take() {
            if let Some(mode) = picker.modes.get(picker.selected) {
                if let Some(override_handle) = &self.policy_override {
                    override_handle.set(Some(*mode));
                }
            }
        }
    }

    pub(in crate::tui) fn thinking_picker_confirm(&mut self) {
        if let Some(picker) = self.thinking_picker.take() {
            if let Some(level) = picker.levels.get(picker.selected) {
                self.pending_model_switch =
                    Some(format!("{}:{}", self.switch_label(), level.as_str()));
            }
        }
    }

    /// The label a model switch must carry: the selected entry for an
    /// external backend, whose displayed model is what the CLI reports.
    fn switch_label(&self) -> String {
        self.selected_model_label
            .clone()
            .unwrap_or_else(|| self.model_label.clone())
    }

    pub(in crate::tui) fn apply_model_switch(
        &mut self,
        label: &str,
        level: ThinkingLevel,
        tier: ServiceTier,
        context_window: Option<u64>,
    ) {
        self.model_label = label.to_string();
        self.thinking_label = (level != ThinkingLevel::Off).then(|| format!(":{}", level.as_str()));
        self.thinking = level;
        self.service_label = (tier != ServiceTier::Auto).then(|| format!("@{}", tier.as_str()));
        self.service_tier = tier;
        self.ctx_limit = context_window
            .filter(|&l| l > 0)
            .unwrap_or(DEFAULT_CTX_LIMIT);
        self.lifecycle.set_context_window(self.ctx_limit);
        self.notify(
            NotifyKind::Info,
            format!(
                "switched to {}{}{} for the next turn",
                label,
                self.thinking_label.as_deref().unwrap_or(""),
                self.service_label.as_deref().unwrap_or("")
            ),
        );
        self.bump_render_epoch();
    }
}
