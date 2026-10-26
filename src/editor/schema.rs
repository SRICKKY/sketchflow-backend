//! Server-side port of the Next.js zod schemas in `@/lib/validation`
//! (`elementSchema`, `presentationConfigSchema`). Like zod's default object
//! parsing, accepted values are returned with unknown keys stripped.

use serde_json::{Map, Value};

use crate::error::AppError;

pub const EMBED_URL_MAX: usize = 2048;

const FILL_STYLES: &[&str] = &["solid", "hachure", "cross-hatch", "none"];
const STROKE_STYLES: &[&str] = &["solid", "dashed", "dotted"];
const ROUNDNESS: &[&str] = &["sharp", "round", "soft"];
const ARROWHEADS: &[&str] = &["none", "arrow", "dot"];
const ARROW_TYPES: &[&str] = &["sharp", "round", "elbow"];
const ROUTING: &[&str] = &["straight", "elbow"];
const BINDING_SIDES: &[&str] = &["n", "s", "e", "w", "center"];
const FONT_FAMILIES: &[&str] = &["sans", "serif", "mono", "sketch"];
const FONT_WEIGHTS: &[&str] = &["normal", "bold"];
const FONT_STYLES: &[&str] = &["normal", "italic"];
const TEXT_ALIGN: &[&str] = &["left", "center", "right"];
const VERTICAL_ALIGN: &[&str] = &["top", "middle", "bottom"];

type Check<T> = Result<T, String>;

/// zod `.max()`/`.min()` count UTF-16 code units, not bytes or scalar values.
fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn is_int(value: f64) -> bool {
    value.fract() == 0.0
}

/// Field-by-field reader that copies only declared keys into `out`.
struct Obj<'a> {
    src: &'a Map<String, Value>,
    out: Map<String, Value>,
    path: String,
}

impl<'a> Obj<'a> {
    fn new(value: &'a Value, path: impl Into<String>) -> Check<Self> {
        let path = path.into();
        let src = value
            .as_object()
            .ok_or_else(|| format!("{path} must be an object"))?;
        Ok(Self {
            src,
            out: Map::new(),
            path,
        })
    }

    fn at(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    /// Returns `None` when absent; `Some(Null)` is surfaced for nullable callers.
    fn field(&self, key: &str, required: bool) -> Check<Option<&'a Value>> {
        match self.src.get(key) {
            None if required => Err(format!("{} is required", self.at(key))),
            None => Ok(None),
            Some(value) => Ok(Some(value)),
        }
    }

    fn number(
        &mut self,
        key: &str,
        required: bool,
        rule: fn(f64) -> bool,
        desc: &str,
    ) -> Check<()> {
        if let Some(value) = self.field(key, required)? {
            let number = value
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| format!("{} must be a finite number", self.at(key)))?;
            if !rule(number) {
                return Err(format!("{} {desc}", self.at(key)));
            }
            self.out.insert(key.into(), value.clone());
        }
        Ok(())
    }

    fn finite(&mut self, key: &str) -> Check<()> {
        self.number(key, true, |_| true, "")
    }

    fn string(&mut self, key: &str, required: bool, min: usize, max: Option<usize>) -> Check<()> {
        if let Some(value) = self.field(key, required)? {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{} must be a string", self.at(key)))?;
            let len = js_len(text);
            if len < min || max.is_some_and(|max| len > max) {
                return Err(format!("{} has an invalid length", self.at(key)));
            }
            self.out.insert(key.into(), value.clone());
        }
        Ok(())
    }

    /// `z.string().min(min).nullable()` (+ `.optional()` when not required).
    fn nullable_string(&mut self, key: &str, required: bool, min: usize) -> Check<()> {
        match self.field(key, required)? {
            Some(Value::Null) => {
                self.out.insert(key.into(), Value::Null);
                Ok(())
            }
            Some(_) => self.string(key, required, min, None),
            None => Ok(()),
        }
    }

    fn one_of(&mut self, key: &str, required: bool, allowed: &[&str]) -> Check<()> {
        if let Some(value) = self.field(key, required)? {
            if !value.as_str().is_some_and(|v| allowed.contains(&v)) {
                return Err(format!(
                    "{} must be one of {}",
                    self.at(key),
                    allowed.join(", ")
                ));
            }
            self.out.insert(key.into(), value.clone());
        }
        Ok(())
    }

    fn boolean(&mut self, key: &str, required: bool) -> Check<()> {
        if let Some(value) = self.field(key, required)? {
            if !value.is_boolean() {
                return Err(format!("{} must be a boolean", self.at(key)));
            }
            self.out.insert(key.into(), value.clone());
        }
        Ok(())
    }

    fn nested(
        &mut self,
        key: &str,
        required: bool,
        nullable: bool,
        parse: fn(&Value, String) -> Check<Value>,
    ) -> Check<()> {
        match self.field(key, required)? {
            Some(Value::Null) if nullable => {
                self.out.insert(key.into(), Value::Null);
            }
            Some(value) => {
                let parsed = parse(value, self.at(key))?;
                self.out.insert(key.into(), parsed);
            }
            None => {}
        }
        Ok(())
    }

    fn points(&mut self, key: &str, min: usize) -> Check<()> {
        let value = self.field(key, true)?.unwrap();
        let items = value
            .as_array()
            .ok_or_else(|| format!("{} must be an array", self.at(key)))?;
        if items.len() < min {
            return Err(format!("{} needs at least {min} points", self.at(key)));
        }
        let mut out = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            out.push(point(item, format!("{}[{index}]", self.at(key)))?);
        }
        self.out.insert(key.into(), Value::Array(out));
        Ok(())
    }

    fn finish(self) -> Value {
        Value::Object(self.out)
    }
}

fn point(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path)?;
    obj.finite("x")?;
    obj.finite("y")?;
    Ok(obj.finish())
}

fn binding(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path)?;
    obj.string("elementId", true, 1, None)?;
    obj.finite("focus")?;
    obj.one_of("side", false, BINDING_SIDES)?;
    Ok(obj.finish())
}

fn fill_clip(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path)?;
    for key in ["x", "y", "width", "height"] {
        obj.finite(key)?;
    }
    obj.one_of("roundness", false, ROUNDNESS)?;
    Ok(obj.finish())
}

fn text_fields(obj: &mut Obj<'_>) -> Check<()> {
    obj.string("text", true, 0, None)?;
    obj.number("fontSize", true, |n| n > 0.0, "must be positive")?;
    obj.one_of("fontFamily", true, FONT_FAMILIES)?;
    obj.one_of("fontWeight", true, FONT_WEIGHTS)?;
    obj.one_of("fontStyle", true, FONT_STYLES)?;
    obj.one_of("textAlign", true, TEXT_ALIGN)
}

fn fill_partition(obj: &mut Obj<'_>) -> Check<()> {
    obj.string("fillParentId", false, 1, None)?;
    obj.nested("fillClip", false, false, fill_clip)
}

fn is_note_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// Validates one `SketchElement`, returning it without unknown keys.
pub fn element(value: &Value, path: &str) -> Check<Value> {
    let mut obj = Obj::new(value, path)?;
    let element_type = obj
        .field("type", true)?
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}.type must be a string"))?
        .to_owned();
    obj.out
        .insert("type".into(), Value::String(element_type.clone()));
    obj.string("id", true, 1, None)?;
    for key in ["x", "y", "width", "height", "rotation"] {
        obj.finite(key)?;
    }
    obj.string("strokeColor", true, 0, None)?;
    obj.string("backgroundColor", true, 0, None)?;
    obj.finite("strokeWidth")?;
    obj.number(
        "opacity",
        true,
        |n| (0.0..=1.0).contains(&n),
        "must be between 0 and 1",
    )?;
    obj.one_of("fillStyle", true, FILL_STYLES)?;
    obj.one_of("strokeStyle", true, STROKE_STYLES)?;
    obj.one_of("roundness", false, ROUNDNESS)?;
    obj.finite("roughness")?;
    obj.finite("seed")?;
    match obj.field("groupId", true)? {
        Some(Value::Null) => {
            obj.out.insert("groupId".into(), Value::Null);
        }
        _ => obj.string("groupId", true, 0, None)?,
    }
    obj.nullable_string("frameId", false, 1)?;
    obj.finite("zIndex")?;
    obj.boolean("locked", true)?;

    match element_type.as_str() {
        "rectangle" => fill_partition(&mut obj)?,
        "ellipse" | "diamond" => {}
        "line" => obj.points("points", 2)?,
        "arrow" => {
            obj.points("points", 2)?;
            obj.one_of("startArrowhead", true, ARROWHEADS)?;
            obj.one_of("endArrowhead", true, ARROWHEADS)?;
            obj.one_of("arrowType", false, ARROW_TYPES)?;
            obj.nested("startBinding", true, true, binding)?;
            obj.nested("endBinding", true, true, binding)?;
            obj.one_of("routing", false, ROUTING)?;
        }
        "freehand" => {
            obj.points("points", 1)?;
            fill_partition(&mut obj)?;
        }
        "text" => {
            text_fields(&mut obj)?;
            obj.one_of("verticalAlign", false, VERTICAL_ALIGN)?;
            obj.nullable_string("containerId", false, 1)?;
            obj.boolean("autoResize", false)?;
        }
        "image" => {
            obj.string("assetId", true, 1, None)?;
            obj.string("mimeType", true, 1, None)?;
            obj.number("naturalWidth", true, |n| n > 0.0, "must be positive")?;
            obj.number("naturalHeight", true, |n| n > 0.0, "must be positive")?;
        }
        "note" => {
            text_fields(&mut obj)?;
            if let Some(date) = obj.field("createdAt", false)? {
                if !date.as_str().is_some_and(is_note_date) {
                    return Err(format!("{path}.createdAt must be YYYY-MM-DD"));
                }
                obj.out.insert("createdAt".into(), date.clone());
            }
        }
        "frame" => obj.string("name", true, 1, Some(80))?,
        "embed" => obj.string("url", true, 0, Some(EMBED_URL_MAX))?,
        _ => {
            return Err(format!(
                "{path}.type is not a supported canvas element type"
            ))
        }
    }
    Ok(obj.finish())
}

/// Validates a list of elements, mapping failures to a 400 with `context`.
pub fn elements(values: &[Value], context: &str) -> Result<Vec<Value>, AppError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| element(value, &format!("elements[{index}]")))
        .collect::<Check<Vec<_>>>()
        .map_err(|detail| AppError::BadRequest(format!("{context}: {detail}")))
}

fn frame(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path.clone())?;
    obj.string("id", true, 1, Some(100))?;
    let name = obj
        .field("name", true)?
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}.name must be a string"))?
        .trim()
        .to_owned();
    if name.is_empty() || js_len(&name) > 80 {
        return Err(format!("{path}.name has an invalid length"));
    }
    obj.out.insert("name".into(), Value::String(name));
    obj.number(
        "order",
        true,
        |n| is_int(n) && (0.0..=100_000.0).contains(&n),
        "must be an integer from 0 to 100000",
    )?;
    obj.finite("x")?;
    obj.finite("y")?;
    for key in ["width", "height"] {
        obj.number(
            key,
            true,
            |n| n > 0.0 && n <= 1_000_000.0,
            "must be positive and at most 1000000",
        )?;
    }
    obj.string("background", false, 0, Some(64))?;
    obj.one_of("transition", false, &["none", "fade", "slide"])?;
    obj.number(
        "durationSeconds",
        false,
        |n| is_int(n) && (1.0..=86_400.0).contains(&n),
        "must be an integer from 1 to 86400",
    )?;
    obj.string("speakerNotes", false, 0, Some(10_000))?;
    Ok(obj.finish())
}

fn easing_curve(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path)?;
    for key in ["x1", "y1", "x2", "y2"] {
        obj.number(
            key,
            true,
            |n| (0.0..=1.0).contains(&n),
            "must be between 0 and 1",
        )?;
    }
    Ok(obj.finish())
}

fn flow(value: &Value, path: String) -> Check<Value> {
    let mut obj = Obj::new(value, path.clone())?;
    obj.one_of(
        "animation",
        false,
        &[
            "smooth-pan-zoom",
            "smart-animate",
            "linear-slide",
            "dissolve",
        ],
    )?;
    obj.one_of(
        "easing",
        false,
        &["ease-in-out", "ease-in", "ease-out", "linear", "custom"],
    )?;
    obj.nested("easingCurve", false, false, easing_curve)?;
    obj.number(
        "transitionDurationSeconds",
        false,
        |n| (0.05..=5.0).contains(&n),
        "must be between 0.05 and 5",
    )?;
    obj.number(
        "delaySeconds",
        false,
        |n| (0.0..=3.0).contains(&n),
        "must be between 0 and 3",
    )?;
    obj.one_of(
        "framingStrategy",
        false,
        &["dynamic-zoom", "fit-bounds", "center-frame"],
    )?;
    obj.boolean("parallaxDepth", false)?;
    obj.boolean("motionBlur", false)?;
    obj.one_of("motionBlurIntensity", false, &["low", "medium", "high"])?;
    obj.one_of("navigationMode", false, &["click-keys", "timed-auto"])?;
    obj.number(
        "navigationIntervalSeconds",
        false,
        |n| (1.0..=300.0).contains(&n),
        "must be between 1 and 300",
    )?;
    if obj.out.get("easing").and_then(Value::as_str) == Some("custom")
        && !obj.out.contains_key("easingCurve")
    {
        return Err(format!("{path}.easingCurve is required for custom easing"));
    }
    Ok(obj.finish())
}

/// Validates a `PresentationConfig`, returning it without unknown keys.
pub fn presentation(value: &Value) -> Result<Value, AppError> {
    let parse = || -> Check<Value> {
        let mut obj = Obj::new(value, "presentation")?;
        let frames = obj
            .field("frames", true)?
            .and_then(Value::as_array)
            .ok_or("presentation.frames must be an array")?;
        if frames.len() > 100 {
            return Err("presentation.frames supports at most 100 frames".into());
        }
        let frames = frames
            .iter()
            .enumerate()
            .map(|(index, item)| frame(item, format!("presentation.frames[{index}]")))
            .collect::<Check<Vec<_>>>()?;
        obj.out.insert("frames".into(), Value::Array(frames));
        obj.boolean("autoPlay", false)?;
        obj.nested("flow", false, false, flow)?;
        Ok(obj.finish())
    };
    parse().map_err(|detail| AppError::BadRequest(format!("Invalid presentation data: {detail}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(element_type: &str) -> Value {
        json!({
            "id": "el-1", "type": element_type, "x": 1, "y": 2.5, "width": 10, "height": 20,
            "rotation": 0, "strokeColor": "#000", "backgroundColor": "transparent",
            "strokeWidth": 2, "opacity": 1, "fillStyle": "solid", "strokeStyle": "solid",
            "roundness": "soft", "roughness": 0, "seed": 42, "groupId": null, "frameId": null,
            "zIndex": 3, "locked": false
        })
    }

    fn with(element_type: &str, extra: Value) -> Value {
        let mut value = base(element_type);
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value
    }

    fn text_extra() -> Value {
        json!({"text":"hi","fontSize":20,"fontFamily":"sans","fontWeight":"normal","fontStyle":"normal","textAlign":"left"})
    }

    #[test]
    fn accepts_every_sketch_element_shape() {
        let clip = json!({"x":0,"y":0,"width":5,"height":5,"roundness":"round"});
        let mut text = text_extra();
        text["verticalAlign"] = json!("middle");
        text["containerId"] = json!("box");
        text["autoResize"] = json!(false);
        let mut note = text_extra();
        note["createdAt"] = json!("2026-10-24");
        let samples = [
            with("rectangle", json!({"fillParentId":"p","fillClip":clip})),
            base("ellipse"),
            base("diamond"),
            with("line", json!({"points":[{"x":0,"y":0},{"x":1,"y":1}]})),
            with(
                "arrow",
                json!({"points":[{"x":0,"y":0},{"x":1,"y":1}],"startArrowhead":"none","endArrowhead":"arrow","arrowType":"elbow","routing":"elbow","startBinding":{"elementId":"a","focus":0.5,"side":"n"},"endBinding":null}),
            ),
            with(
                "freehand",
                json!({"points":[{"x":0,"y":0}],"fillClip":clip}),
            ),
            with("text", text),
            with(
                "image",
                json!({"assetId":"asset","mimeType":"image/png","naturalWidth":10,"naturalHeight":10}),
            ),
            with("note", note),
            with("frame", json!({"name":"Frame 1"})),
            with("embed", json!({"url":"https://example.com"})),
        ];
        for sample in samples {
            let parsed = element(&sample, "el").unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(parsed, sample);
        }
    }

    #[test]
    fn strips_unknown_keys_like_zod() {
        let mut value = with(
            "line",
            json!({"points":[{"x":0,"y":0,"pressure":1},{"x":1,"y":1}]}),
        );
        value["evil"] = json!("<script>");
        let parsed = element(&value, "el").unwrap();
        assert!(parsed.get("evil").is_none());
        assert!(parsed["points"][0].get("pressure").is_none());
    }

    #[test]
    fn rejects_invalid_elements() {
        let mut missing_group = base("ellipse");
        missing_group.as_object_mut().unwrap().remove("groupId");
        let invalid = [
            missing_group,
            with("ellipse", json!({"opacity": 2})),
            with("ellipse", json!({"roundness": null})),
            with("ellipse", json!({"frameId": ""})),
            with("ellipse", json!({"fillStyle": "zigzag"})),
            with("line", json!({"points":[{"x":0,"y":0}]})),
            with(
                "arrow",
                json!({"points":[{"x":0,"y":0},{"x":1,"y":1}],"startArrowhead":"none","endArrowhead":"arrow","endBinding":null}),
            ),
            with(
                "text",
                json!({"text":"x","fontSize":0,"fontFamily":"sans","fontWeight":"normal","fontStyle":"normal","textAlign":"left"}),
            ),
            with(
                "image",
                json!({"assetId":"","mimeType":"image/png","naturalWidth":1,"naturalHeight":1}),
            ),
            with("note", {
                let mut n = text_extra();
                n["createdAt"] = json!("24-10-2026");
                n
            }),
            with("frame", json!({"name": "x".repeat(81)})),
            with("embed", json!({"url": "x".repeat(EMBED_URL_MAX + 1)})),
            base("script"),
            json!("not an object"),
        ];
        for value in invalid {
            assert!(element(&value, "el").is_err(), "{value}");
        }
    }

    #[test]
    fn validates_presentation_config() {
        let valid = json!({
            "frames":[{"id":"f","name":"  Intro ","order":0,"x":0,"y":0,"width":100,"height":50,
                "background":"#fff","transition":"fade","durationSeconds":5,"speakerNotes":"hi","junk":1}],
            "autoPlay": true,
            "flow": {"animation":"dissolve","easing":"custom","easingCurve":{"x1":0,"y1":0,"x2":1,"y2":1},
                "transitionDurationSeconds":0.5,"delaySeconds":0,"navigationMode":"timed-auto","navigationIntervalSeconds":3}
        });
        let parsed = presentation(&valid).unwrap();
        assert_eq!(parsed["frames"][0]["name"], "Intro");
        assert!(parsed["frames"][0].get("junk").is_none());
        for invalid in [
            json!({}),
            json!({"frames":[{"id":"f","name":" ","order":0,"x":0,"y":0,"width":1,"height":1}]}),
            json!({"frames":[{"id":"f","name":"a","order":1.5,"x":0,"y":0,"width":1,"height":1}]}),
            json!({"frames":[{"id":"f","name":"a","order":0,"x":0,"y":0,"width":0,"height":1}]}),
            json!({"frames":[],"flow":{"easing":"custom"}}),
            json!({"frames":[],"flow":{"delaySeconds":4}}),
        ] {
            assert!(presentation(&invalid).is_err(), "{invalid}");
        }
    }
}
