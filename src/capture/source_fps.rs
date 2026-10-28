/// Reads the current video source frame rate from the TV display service.
pub async fn detect_source_fps() -> Option<f64> {
    let value = crate::luna::query("luna://com.webos.service.tv.display/getVideoInfo", "{}")
        .await
        .ok()?;
    source_fps_from_video_info(&value)
}

fn source_fps_from_video_info(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Object(fields) => fields
            .get("frameRate")
            .and_then(parse_frame_rate)
            .or_else(|| fields.values().find_map(source_fps_from_video_info)),
        serde_json::Value::Array(values) => values.iter().find_map(source_fps_from_video_info),
        _ => None,
    }
}

fn parse_frame_rate(value: &serde_json::Value) -> Option<f64> {
    if let Some(value) = value.as_f64() {
        return value.is_finite().then_some(value);
    }
    let value = value.as_str()?.trim();
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.trim().parse::<f64>().ok()?;
        let denominator = denominator.trim().parse::<f64>().ok()?;
        return (denominator != 0.0).then_some(numerator / denominator);
    }
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_fractional_video_frame_rate() {
        let video_info = serde_json::json!({"payload": {"frameRate": "24000/1001"}});
        assert!((source_fps_from_video_info(&video_info).unwrap() - 23.976).abs() < 0.001);
    }
}
