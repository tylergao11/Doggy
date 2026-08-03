//! Canonical session-mode enum shared between the agent and pager.
//!
//! ACP carries the mode as an opaque session mode id (`Arc<str>`).
//! This enum is the typed counterpart both crates parse into / serialize
//! out of.
//!
//! Doggy product surface (Shift+Tab) is two **work postures**:
//! - [`SessionMode::Default`] — Auto: ordinary chat with full tool permission
//! - [`SessionMode::Goal`] — run until goal verification (acceptance criteria) passes
//!
//! Plan mode has been removed from the product. The legacy wire id `plan`
//! (and `ask`) normalize to [`SessionMode::Default`] so old clients do not
//! re-enter a deleted posture.

/// Wire representation is the snake-cased variant name (`default`, `goal`)
/// via [`strum`]. Unknown ids (including legacy `plan` / `ask`) parse to
/// [`SessionMode::Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SessionMode {
    Default,
    Goal,
}

impl SessionMode {
    /// Parse from the wire id. Unknown and retired ids fall back to
    /// [`SessionMode::Default`] (Auto).
    pub fn from_id(id: &str) -> Self {
        match id {
            "goal" => Self::Goal,
            // Retired product modes — never re-enter Plan.
            "plan" | "ask" | "default" => Self::Default,
            other => other.parse().unwrap_or(Self::Default),
        }
    }

    /// The canonical wire id for this mode (snake_case).
    pub fn as_id(&self) -> &'static str {
        self.into()
    }

    pub fn is_goal(&self) -> bool {
        matches!(self, Self::Goal)
    }

    /// Product label for Shift+Tab / mode chip (Auto / Goal).
    pub fn product_label(self) -> &'static str {
        match self {
            Self::Goal => "Goal",
            Self::Default => "Auto",
        }
    }

    /// Collapse into the two resident product postures.
    pub fn as_product(self) -> Self {
        match self {
            Self::Goal => Self::Goal,
            Self::Default => Self::Default,
        }
    }

    /// Shift+Tab ring: Auto → Goal → Auto.
    pub fn product_cycle(self) -> Self {
        match self.as_product() {
            Self::Default => Self::Goal,
            Self::Goal => Self::Default,
        }
    }

    /// Whether this posture auto-runs tools with full permission.
    /// Both Auto and Goal use full permission.
    pub fn full_tool_permission(self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_known_ids() {
        for &id in &["default", "goal"] {
            let mode = SessionMode::from_id(id);
            assert_eq!(mode.as_id(), id, "round-trip failed for {id}");
        }
    }

    #[test]
    fn retired_plan_and_ask_normalize_to_auto() {
        assert_eq!(SessionMode::from_id("plan"), SessionMode::Default);
        assert_eq!(SessionMode::from_id("ask"), SessionMode::Default);
        assert_eq!(SessionMode::from_id("PLAN"), SessionMode::Default);
    }

    #[test]
    fn unknown_id_falls_back_to_default() {
        assert_eq!(SessionMode::from_id("browser_use"), SessionMode::Default);
        assert_eq!(SessionMode::from_id(""), SessionMode::Default);
    }

    #[test]
    fn is_goal_only_for_goal_variant() {
        assert!(SessionMode::Goal.is_goal());
        assert!(!SessionMode::Default.is_goal());
    }

    #[test]
    fn product_cycle_is_auto_goal() {
        assert_eq!(SessionMode::Default.product_cycle(), SessionMode::Goal);
        assert_eq!(SessionMode::Goal.product_cycle(), SessionMode::Default);
    }

    #[test]
    fn product_cycle_two_steps_returns() {
        let start = SessionMode::Default;
        let m = start.product_cycle().product_cycle();
        assert_eq!(m, start);
    }

    #[test]
    fn product_labels() {
        assert_eq!(SessionMode::Default.product_label(), "Auto");
        assert_eq!(SessionMode::Goal.product_label(), "Goal");
    }

    #[test]
    fn full_tool_permission_always() {
        assert!(SessionMode::Default.full_tool_permission());
        assert!(SessionMode::Goal.full_tool_permission());
    }
}
