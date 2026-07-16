pub(crate) fn infer_data_fields(source: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut i = 0;

    while i < source.len() {
        let remaining = &source[i..];
        if remaining.starts_with("data.at(") {
            if let Some((field, consumed)) = parse_first_string_arg(remaining, "data.at(") {
                push_unique(&mut fields, field);
                i += consumed;
                continue;
            }
        } else if remaining.starts_with("data.") {
            if let Some((field, consumed)) = parse_data_path(remaining) {
                push_unique(&mut fields, field);
                i += consumed;
                continue;
            }
        } else if remaining.starts_with("field(")
            && let Some((field, consumed)) = parse_first_string_arg(remaining, "field(")
        {
            push_unique(&mut fields, field);
            i += consumed;
            continue;
        }

        i += remaining.chars().next().map(char::len_utf8).unwrap_or(1);
    }

    fields
}

pub(crate) fn parse_data_path(source: &str) -> Option<(String, usize)> {
    let mut offset = "data.".len();
    let (first, consumed) = parse_identifier(&source[offset..])?;
    if first == "at" {
        return None;
    }
    offset += consumed;

    let mut parts = vec![first];
    while source[offset..].starts_with('.') {
        let next_offset = offset + 1;
        let Some((part, consumed)) = parse_identifier(&source[next_offset..]) else {
            break;
        };
        parts.push(part);
        offset = next_offset + consumed;
    }

    Some((parts.join("."), offset))
}

pub(crate) fn parse_identifier(source: &str) -> Option<(String, usize)> {
    let mut chars = source.char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }

    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    Some((source[..end].to_string(), end))
}

pub(crate) fn parse_first_string_arg(source: &str, prefix: &str) -> Option<(String, usize)> {
    let mut offset = prefix.len();
    while let Some(ch) = source[offset..].chars().next()
        && ch.is_whitespace()
    {
        offset += ch.len_utf8();
    }

    let quote = source[offset..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    offset += quote.len_utf8();

    let mut value = String::new();
    let mut escaped = false;
    for (idx, ch) in source[offset..].char_indices() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some((value, offset + idx + ch.len_utf8()));
        } else {
            value.push(ch);
        }
    }

    None
}

pub(crate) fn push_unique(fields: &mut Vec<String>, field: String) {
    if field.trim().is_empty() || fields.iter().any(|existing| existing == &field) {
        return;
    }
    fields.push(field);
}

pub(crate) fn sample_data_json(fields: &[String]) -> String {
    if fields.is_empty() {
        return "{}".to_string();
    }

    let mut root = serde_json::Map::new();
    for field in fields {
        let parts: Vec<&str> = field.split('.').filter(|part| !part.is_empty()).collect();
        insert_json_path(&mut root, &parts);
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn insert_json_path(
    map: &mut serde_json::Map<String, serde_json::Value>,
    parts: &[&str],
) {
    let Some((head, tail)) = parts.split_first() else {
        return;
    };

    if tail.is_empty() {
        map.entry((*head).to_string())
            .or_insert_with(|| serde_json::Value::String(String::new()));
        return;
    }

    let entry = map
        .entry((*head).to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(child) = entry.as_object_mut() {
        insert_json_path(child, tail);
    }
}
