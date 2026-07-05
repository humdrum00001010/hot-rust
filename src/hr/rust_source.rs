use std::error::Error;

pub(crate) fn function_item_has_no_body(source: &str, fn_start: usize) -> bool {
    let Some(brace) = source[fn_start..].find('{').map(|pos| fn_start + pos) else {
        return true;
    };
    let header = &source[fn_start..brace];
    header.contains(';') || header.contains(" = ")
}

pub(crate) fn function_has_type_parameters(signature: &str, function_name: &str) -> bool {
    let Some(fn_pos) = signature.find("fn ") else {
        return false;
    };
    let name_start = fn_pos + 3;
    let Some(name_offset) = signature[name_start..].find(function_name) else {
        return false;
    };
    let mut index = name_start + name_offset + function_name.len();
    let bytes = signature.as_bytes();
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    bytes.get(index) == Some(&b'<')
}

pub(crate) fn direct_same_impl_callees(body: &str) -> Vec<String> {
    collect_body_call_names(body, |bytes, index, names| {
        if bytes[index..].starts_with(b"self.") {
            if let Some((name, next)) = method_call_name_after(bytes, index + 5) {
                names.push(name);
                return Some(next);
            }
        }
        if bytes[index..].starts_with(b"Self::") {
            if let Some((name, next)) = method_call_name_after(bytes, index + 6) {
                names.push(name);
                return Some(next);
            }
        }
        None
    })
}

pub(crate) fn dot_method_callees(body: &str) -> Vec<String> {
    collect_body_call_names(body, |bytes, index, names| {
        if bytes[index] == b'.' {
            if let Some((name, next)) = method_call_name_after(bytes, index + 1) {
                names.push(name);
                return Some(next);
            }
        }
        None
    })
}

pub(crate) fn function_callees(body: &str) -> Vec<String> {
    collect_body_call_names(body, |bytes, mut index, names| {
        let first = bytes[index];
        if !(first == b'_' || first.is_ascii_alphabetic()) {
            return None;
        }
        let start = index;
        index += 1;
        while bytes
            .get(index)
            .copied()
            .map(is_ident_byte)
            .unwrap_or(false)
        {
            index += 1;
        }
        let name_end = index;
        while bytes
            .get(index)
            .copied()
            .map(|byte| byte.is_ascii_whitespace())
            .unwrap_or(false)
        {
            index += 1;
        }
        if bytes.get(index) == Some(&b'(') && previous_non_ws(bytes, start) != Some(b'.') {
            if let Ok(name) = std::str::from_utf8(&bytes[start..name_end]) {
                names.push(name.to_string());
            }
        }
        Some(index)
    })
}

#[derive(Clone, Copy)]
enum BodyScanState {
    Normal,
    LineComment,
    BlockComment(usize),
    String { escaped: bool },
    Char { escaped: bool },
    RawString { hashes: usize },
}

fn collect_body_call_names(
    body: &str,
    mut visit_code: impl FnMut(&[u8], usize, &mut Vec<String>) -> Option<usize>,
) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut names = Vec::new();
    let mut state = BodyScanState::Normal;
    let mut index = 0usize;
    while index < bytes.len() {
        match state {
            BodyScanState::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = BodyScanState::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = BodyScanState::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = BodyScanState::RawString { hashes };
                    index += len;
                    continue;
                }
                if bytes[index] == b'"' {
                    state = BodyScanState::String { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index] == b'\'' && looks_like_char_start(bytes, index) {
                    state = BodyScanState::Char { escaped: false };
                    index += 1;
                    continue;
                }
                if let Some(next) = visit_code(bytes, index, &mut names) {
                    index = next;
                } else {
                    index += 1;
                }
            }
            BodyScanState::LineComment => {
                if bytes[index] == b'\n' {
                    state = BodyScanState::Normal;
                }
                index += 1;
            }
            BodyScanState::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = BodyScanState::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = BodyScanState::Normal;
                    } else {
                        state = BodyScanState::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            BodyScanState::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = BodyScanState::String { escaped: false };
                } else if byte == b'\\' {
                    state = BodyScanState::String { escaped: true };
                } else if byte == b'"' {
                    state = BodyScanState::Normal;
                }
                index += 1;
            }
            BodyScanState::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = BodyScanState::Char { escaped: false };
                } else if byte == b'\\' {
                    state = BodyScanState::Char { escaped: true };
                } else if byte == b'\'' {
                    state = BodyScanState::Normal;
                }
                index += 1;
            }
            BodyScanState::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = BodyScanState::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn previous_non_ws(bytes: &[u8], index: usize) -> Option<u8> {
    let mut index = index.checked_sub(1)?;
    loop {
        let byte = *bytes.get(index)?;
        if !byte.is_ascii_whitespace() {
            return Some(byte);
        }
        let Some(next) = index.checked_sub(1) else {
            return None;
        };
        index = next;
    }
}

fn method_call_name_after(bytes: &[u8], mut index: usize) -> Option<(String, usize)> {
    let start = index;
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    let name = std::str::from_utf8(&bytes[start..index]).ok()?.to_string();
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    (bytes.get(index) == Some(&b'(')).then_some((name, index))
}

pub(crate) fn function_name_from_signature(signature: &str) -> Option<String> {
    let fn_pos = signature.find("fn ")? + 3;
    let bytes = signature.as_bytes();
    let mut index = fn_pos;
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    let start = index;
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    Some(String::from_utf8(bytes[start..index].to_vec()).ok()?)
}

pub(crate) fn function_params_open(signature: &str) -> Option<usize> {
    let fn_pos = signature.find("fn ")? + 3;
    let bytes = signature.as_bytes();
    let mut index = fn_pos;
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    signature[index..].find('(').map(|offset| index + offset)
}

pub(crate) fn prune_function_bodies_in_source(source: &str) -> (String, usize) {
    prune_function_bodies_in_source_except(source, &[])
}

pub(crate) fn prune_function_bodies_in_source_except(
    source: &str,
    skip_names: &[String],
) -> (String, usize) {
    let mut ranges = Vec::new();
    let mut search = 0usize;
    for fn_start in function_item_positions(source) {
        if fn_start < search {
            continue;
        }
        if !looks_like_function_item(source, fn_start) {
            continue;
        }
        let Some(brace) = source[fn_start..].find('{').map(|pos| fn_start + pos) else {
            continue;
        };
        let header = &source[fn_start..brace];
        if header.contains(';') || header.contains(" = ") {
            continue;
        }
        if function_prefix_line(source, fn_start).contains("const") {
            continue;
        }
        if function_name_at(source, fn_start)
            .as_ref()
            .map(|name| skip_names.iter().any(|skip| skip == name))
            .unwrap_or(false)
        {
            continue;
        }
        let Some(end) = matching_code_brace(source, brace) else {
            continue;
        };
        ranges.push((brace + 1, end));
        search = end + 1;
    }

    if ranges.is_empty() {
        return (source.to_string(), 0);
    }

    let mut out = source.to_string();
    for (start, end) in ranges.iter().rev() {
        let indent = body_indent_for(&out, *start);
        out.replace_range(
            *start..*end,
            &format!("\n{indent}unimplemented!(\"hot-rust pruned shadow body\")\n"),
        );
    }
    let count = ranges.len();
    (out, count)
}

pub(crate) fn function_name_at(source: &str, fn_start: usize) -> Option<String> {
    let bytes = source.as_bytes();
    if !bytes.get(fn_start..)?.starts_with(b"fn") {
        return None;
    }
    let mut index = fn_start + 2;
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    let start = index;
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    std::str::from_utf8(&bytes[start..index])
        .ok()
        .map(ToString::to_string)
}

pub(crate) fn matching_code_brace(source: &str, open: usize) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        LineComment,
        BlockComment(usize),
        String { escaped: bool },
        Char { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut state = State::Normal;
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = State::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = State::RawString { hashes };
                    index += len;
                    continue;
                }
                match bytes[index] {
                    b'"' => {
                        state = State::String { escaped: false };
                        index += 1;
                    }
                    b'\'' if looks_like_char_start(bytes, index) => {
                        state = State::Char { escaped: false };
                        index += 1;
                    }
                    b'{' => {
                        depth += 1;
                        index += 1;
                    }
                    b'}' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(index);
                        }
                        index += 1;
                    }
                    _ => index += 1,
                }
            }
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::String { escaped: false };
                } else if byte == b'\\' {
                    state = State::String { escaped: true };
                } else if byte == b'"' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::Char { escaped: false };
                } else if byte == b'\\' {
                    state = State::Char { escaped: true };
                } else if byte == b'\'' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = State::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    None
}

pub(crate) fn function_item_positions(source: &str) -> Vec<usize> {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        LineComment,
        BlockComment(usize),
        String { escaped: bool },
        Char { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = source.as_bytes();
    let mut positions = Vec::new();
    let mut state = State::Normal;
    let mut index = 0usize;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = State::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = State::RawString { hashes };
                    index += len;
                    continue;
                }
                if bytes[index] == b'"' {
                    state = State::String { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index] == b'\'' && looks_like_char_start(bytes, index) {
                    state = State::Char { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index..].starts_with(b"fn ")
                    && index
                        .checked_sub(1)
                        .map(|pos| !is_ident_byte(bytes[pos]))
                        .unwrap_or(true)
                    && bytes
                        .get(index + 2)
                        .map(|byte| byte.is_ascii_whitespace())
                        .unwrap_or(false)
                {
                    positions.push(index);
                    index += 3;
                    continue;
                }
                index += 1;
            }
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::String { escaped: false };
                } else if byte == b'\\' {
                    state = State::String { escaped: true };
                } else if byte == b'"' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::Char { escaped: false };
                } else if byte == b'\\' {
                    state = State::Char { escaped: true };
                } else if byte == b'\'' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = State::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    positions
}

pub(crate) fn raw_string_start(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    if bytes.get(offset) == Some(&b'b') {
        offset += 1;
    }
    if bytes.get(offset) != Some(&b'r') {
        return None;
    }
    offset += 1;
    let mut hashes = 0usize;
    while bytes.get(offset + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(offset + hashes) == Some(&b'"') {
        Some((hashes, offset + hashes + 1))
    } else {
        None
    }
}

pub(crate) fn looks_like_char_start(bytes: &[u8], index: usize) -> bool {
    if index + 2 >= bytes.len() {
        return false;
    }
    if let Some(next) = bytes.get(index + 1).copied() {
        if (next == b'_' || next.is_ascii_alphabetic()) && bytes.get(index + 2) != Some(&b'\'') {
            return false;
        }
    }
    let prev = index
        .checked_sub(1)
        .and_then(|pos| bytes.get(pos).copied())
        .unwrap_or(b' ');
    if is_ident_byte(prev) {
        return false;
    }
    true
}

pub(crate) fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

pub(crate) fn looks_like_function_item(source: &str, fn_start: usize) -> bool {
    let prefix = function_prefix_line(source, fn_start);
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return true;
    }
    let allowed = [
        "pub",
        "pub(crate)",
        "pub(super)",
        "async",
        "const",
        "unsafe",
        "extern",
        "default",
    ];
    trimmed
        .split_whitespace()
        .all(|part| allowed.contains(&part) || part.starts_with("pub(") || part.starts_with('"'))
}

pub(crate) fn function_prefix_line(source: &str, fn_start: usize) -> &str {
    let line_start = source[..fn_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    &source[line_start..fn_start]
}

pub(crate) fn body_indent_for(source: &str, body_start: usize) -> String {
    let line_start = source[..body_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let prefix = source[line_start..body_start]
        .chars()
        .take_while(|ch| ch.is_whitespace() && *ch != '\n')
        .collect::<String>();
    format!("{prefix}    ")
}

pub(crate) fn method_receiver(signature: &str) -> Option<String> {
    let open = signature.find('(')?;
    let mut depth = 0usize;
    let mut end = None;
    for (offset, ch) in signature[open + 1..].char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                if depth == 0 {
                    if ch == ')' {
                        end = Some(open + 1 + offset);
                        break;
                    }
                } else {
                    depth -= 1;
                }
            }
            ',' if depth == 0 => {
                end = Some(open + 1 + offset);
                break;
            }
            _ => {}
        }
    }
    let first = signature[open + 1..end?].trim();
    let normalized = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let is_receiver = matches!(normalized.as_str(), "self" | "mut self")
        || normalized.starts_with("&self")
        || normalized.starts_with("&mut self")
        || normalized.starts_with("self:")
        || normalized.starts_with("mut self:");
    is_receiver.then_some(normalized)
}

pub(crate) fn containing_impl_type(source: &str, signature_start: usize) -> Option<String> {
    let mut best = None;
    for (impl_start, _) in source[..signature_start].match_indices("impl ") {
        let brace = source[impl_start..].find('{')? + impl_start;
        if brace >= signature_start {
            continue;
        }
        let close = matching_brace(source, brace)?;
        if close > signature_start {
            let header = source[impl_start..brace].trim();
            if let Some(impl_type) = impl_type_from_header(header) {
                best = Some((impl_start, impl_type));
            }
        }
    }
    best.map(|(_, impl_type)| impl_type)
}

pub(crate) fn impl_type_from_header(header: &str) -> Option<String> {
    let mut rest = header.strip_prefix("impl")?.trim();
    if rest.starts_with('<') {
        let generic_end = matching_angle(rest, 0)?;
        rest = rest[generic_end + 1..].trim();
    }
    if let Some(for_index) = rest.rfind(" for ") {
        rest = rest[for_index + " for ".len()..].trim();
    }
    rest = rest.split(" where ").next().unwrap_or(rest).trim();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

pub(crate) fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn matching_angle(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn matching_paren(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn split_top_level_commas(source: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    for (index, ch) in source.char_indices() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            ',' if paren == 0 && bracket == 0 && angle == 0 => {
                parts.push(source[start..index].to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(source[start..].to_string());
    parts
}

pub(crate) fn param_binding_name(param: &str) -> Result<String, Box<dyn Error>> {
    let Some((pattern, _ty)) = param.split_once(':') else {
        return Err(format!("parameter has no type annotation: `{param}`").into());
    };
    let name = pattern
        .split(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .filter(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "mut" | "ref"))
        .next_back()
        .ok_or_else(|| format!("could not extract parameter binding from `{param}`"))?;
    Ok(name.to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedFunction {
    pub(crate) signature: String,
    pub(crate) body: String,
    pub(crate) signature_start: usize,
    pub(crate) body_start: usize,
    pub(crate) body_end: usize,
}

pub(crate) fn extract_function(source: &str, symbol: &str) -> Option<ParsedFunction> {
    for fn_start in function_item_positions(source) {
        if !looks_like_function_item(source, fn_start) {
            continue;
        }
        if function_name_at(source, fn_start).as_deref() != Some(symbol) {
            continue;
        }
        return extract_function_at(source, fn_start);
    }
    None
}

pub(crate) fn extract_function_at(source: &str, fn_start: usize) -> Option<ParsedFunction> {
    let line_start = source[..fn_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let line_prefix = &source[line_start..fn_start];
    let signature_start = if line_prefix.trim().is_empty() {
        fn_start
    } else {
        line_start + line_prefix.len() - line_prefix.trim_start().len()
    };
    let brace = source[fn_start..].find('{')? + fn_start;
    let end = matching_code_brace(source, brace)?;
    Some(ParsedFunction {
        signature: source[signature_start..brace].trim().to_string(),
        body: source[brace + 1..end].to_string(),
        signature_start,
        body_start: brace + 1,
        body_end: end,
    })
}

pub(crate) fn patch_signature(
    old_symbol: &str,
    patch_symbol: &str,
    signature: &str,
) -> Result<String, Box<dyn Error>> {
    let needle = format!("fn {old_symbol}");
    let replacement = format!("fn {patch_symbol}");
    let renamed = signature.replacen(&needle, &replacement, 1);
    if renamed == signature {
        return Err(format!("signature does not contain `{needle}`: {signature}").into());
    }
    let trimmed = renamed.trim_start();
    if trimmed.starts_with("pub ") || trimmed.starts_with("pub(") {
        Ok(renamed)
    } else {
        Ok(format!("pub {renamed}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn direct_same_impl_callees_ignore_non_code() {
        let body = r##"
            self.draw_text();
            Self::node_z_sort_key();
            other.method();
            let _ = "self.string_call()";
            let _ = r#"Self::raw_string_call()"#;
            // self.line_comment_call()
            /* Self::block_comment_call() */
        "##;

        assert_eq!(
            direct_same_impl_callees(body),
            names(&["draw_text", "node_z_sort_key"])
        );
    }

    #[test]
    fn dot_method_callees_ignore_non_code() {
        let body = r##"
            img.transform.effective_image_bbox(&node.bbox);
            self.output.push_str("</g>\n");
            let _ = "value.fake_method()";
            let _ = r#"raw.fake_raw()"#;
            // value.fake_comment()
            /* value.fake_block() */
        "##;

        assert_eq!(
            dot_method_callees(body),
            names(&["effective_image_bbox", "push_str"])
        );
    }

    #[test]
    fn function_callees_skip_dot_methods_and_non_code() {
        let body = r##"
            let color = color_to_svg(bg.background_color);
            self.output.push_str(&escape_xml(label));
            format!("{}");
            let _ = "string_call()";
            let _ = r#"raw_call()"#;
            // line_comment_call()
            /* block_comment_call() */
        "##;

        assert_eq!(
            function_callees(body),
            names(&["color_to_svg", "escape_xml"])
        );
    }
}
