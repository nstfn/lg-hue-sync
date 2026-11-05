use anyhow::{anyhow, Context, Result};
use serde_json::Value;

const POWER_STATE_URI: &str = "luna://com.webos.service.tvpower/power/getPowerState";

/// Panel state is unavailable in standby or for an unknown power state.
pub async fn display_enabled() -> Result<Option<bool>> {
    let response = crate::luna::query(POWER_STATE_URI, "{}").await?;
    Ok(parse_display_state(&response))
}

fn parse_display_state(response: &Value) -> Option<bool> {
    match response
        .get("state")?
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "active" | "on" => Some(true),
        "screen off" => Some(false),
        _ => None,
    }
}

fn display_uri(enabled: bool) -> &'static str {
    if enabled {
        "luna://com.webos.service.tvpower/power/turnOnScreen"
    } else {
        "luna://com.webos.service.tvpower/power/turnOffScreen"
    }
}

pub async fn set_display(enabled: bool) -> Result<Option<bool>> {
    let response = crate::luna::query(display_uri(enabled), "{}").await?;
    match parse_display_state(&response) {
        Some(state) => Ok(Some(state)),
        None => display_enabled().await,
    }
}

/// Both readings of one `getPowerState` reply, so one poller serves power following and the
/// panel state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PowerState {
    /// `Some(true)` active, `Some(false)` standby or screen off, `None` an unknown state.
    pub active: Option<bool>,
    /// Panel state; `None` in standby or for an unknown state (see `display_enabled`).
    pub display: Option<bool>,
}

pub async fn power_state() -> Result<PowerState> {
    let response = crate::luna::query(POWER_STATE_URI, "{}")
        .await
        .context("webOS TV power query failed")?;
    parse_power_state(&response)
}

pub async fn is_active() -> Result<Option<bool>> {
    Ok(power_state().await?.active)
}

fn parse_power_state(response: &Value) -> Result<PowerState> {
    if response.get("returnValue") == Some(&Value::Bool(false)) {
        return Err(anyhow!("webOS TV power query failed"));
    }
    Ok(PowerState {
        active: response
            .get("state")
            .and_then(Value::as_str)
            .and_then(parse_state),
        display: parse_display_state(response),
    })
}

fn parse_state(state: &str) -> Option<bool> {
    match state.trim().to_ascii_lowercase().as_str() {
        "active" | "on" => Some(true),
        "active standby" | "standby" | "suspend" | "screen off" | "off" | "screen saver" => {
            Some(false)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_state_and_commands_are_separate_from_standby() {
        for (state, expected) in [
            ("Active", Some(true)),
            ("Screen Off", Some(false)),
            ("Active Standby", None),
            ("Suspend", None),
            ("unknown", None),
        ] {
            assert_eq!(
                parse_display_state(&serde_json::json!({"state": state})),
                expected
            );
        }
        assert_eq!(parse_display_state(&serde_json::json!({})), None);
        assert!(display_uri(true).ends_with("/turnOnScreen"));
        assert!(display_uri(false).ends_with("/turnOffScreen"));
    }

    #[test]
    fn maps_known_webos_power_states_and_preserves_unknown() {
        assert_eq!(parse_state("Active"), Some(true));
        assert_eq!(parse_state("on"), Some(true));
        for state in ["Active Standby", "Suspend", "Screen Off", "off"] {
            assert_eq!(parse_state(state), Some(false));
        }
        assert_eq!(parse_state("unrecognized"), None);
    }

    #[test]
    fn one_reply_gives_power_and_panel_state() {
        let parse = |reply| parse_power_state(&reply).unwrap();
        assert_eq!(
            parse(serde_json::json!({"returnValue": true, "state": "Active"})),
            PowerState {
                active: Some(true),
                display: Some(true)
            }
        );
        assert_eq!(
            parse(serde_json::json!({"state": "Screen Off"})),
            PowerState {
                active: Some(false),
                display: Some(false)
            }
        );
        assert_eq!(
            parse(serde_json::json!({"state": "Active Standby"})),
            PowerState {
                active: Some(false),
                display: None
            }
        );
        assert_eq!(parse(serde_json::json!({})), PowerState::default());
        assert!(parse_power_state(&serde_json::json!({"returnValue": false})).is_err());
    }
}
