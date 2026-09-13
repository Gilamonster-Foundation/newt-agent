//! Named theme preferences, sharing the runtime role vocabulary.
use super::{parse_color, role_from_name, role_name, Role, Theme, ALL_ROLES};
use crossterm::style::{Attribute, Color};
use std::path::PathBuf;

const FLAGS: &[(&str, Attribute)] = &[
    ("bold", Attribute::Bold),
    ("dim", Attribute::Dim),
    ("italic", Attribute::Italic),
    ("underline", Attribute::Underlined),
    ("reverse", Attribute::Reverse),
    ("strike", Attribute::CrossedOut),
];

pub fn color_name(color: Color) -> String {
    match color {
        Color::Reset => "default".into(),
        Color::Black => "black".into(),
        Color::DarkRed => "red".into(),
        Color::Red => "light-red".into(),
        Color::DarkGreen => "green".into(),
        Color::Green => "light-green".into(),
        Color::DarkYellow => "yellow".into(),
        Color::Yellow => "light-yellow".into(),
        Color::DarkBlue => "blue".into(),
        Color::Blue => "light-blue".into(),
        Color::DarkMagenta => "magenta".into(),
        Color::Magenta => "light-magenta".into(),
        Color::DarkCyan => "cyan".into(),
        Color::Cyan => "light-cyan".into(),
        Color::Grey => "gray".into(),
        Color::DarkGrey => "dark-gray".into(),
        Color::White => "white".into(),
        Color::AnsiValue(n) => n.to_string(),
        Color::Rgb { r, g, b } => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

pub fn builtins() -> Vec<Theme> {
    vec![
        Theme::builtin(),
        Theme::builtin().overlaid(
            "daylight",
            &[
                (Role::Text, Color::Black),
                (Role::AgentText, Color::Black),
                (Role::HumanText, Color::DarkBlue),
                (Role::InlineCode, Color::Black),
                (Role::MarkdownStrong, Color::Black),
                (Role::MarkdownHeading, Color::DarkBlue),
                (Role::MarkdownItalic, Color::Black),
                (Role::Dim, Color::DarkGrey),
                (Role::Spill, Color::DarkGrey),
                (Role::SelectedValue, Color::DarkBlue),
                (Role::ModalTitle, Color::DarkBlue),
            ],
        ),
        Theme::builtin().overlaid(
            "phosphor",
            &[
                (Role::AgentText, Color::Green),
                (Role::HumanText, Color::Cyan),
                (Role::Text, Color::Green),
                (Role::Accent, Color::Green),
            ],
        ),
    ]
}

fn directory() -> Result<PathBuf, String> {
    crate::settings::settings_path()
        .and_then(|p| p.parent().map(|p| p.join("themes")))
        .ok_or_else(|| "Cannot resolve user theme directory".into())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn encode(theme: &Theme) -> Result<String, String> {
    let mut doc = toml::Table::new();
    doc.insert("name".into(), theme.name.clone().into());
    let mut roles = toml::Table::new();
    for role in ALL_ROLES {
        let mut row = toml::Table::new();
        row.insert("color".into(), color_name(theme.color(*role)).into());
        for (name, attr) in FLAGS {
            row.insert(
                (*name).into(),
                theme.style(*role).attributes.has(*attr).into(),
            );
        }
        roles.insert(role_name(*role).into(), row.into());
    }
    doc.insert("roles".into(), roles.into());
    toml::to_string_pretty(&doc).map_err(|e| e.to_string())
}

fn decode(text: &str) -> Result<Theme, String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let name = doc
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Theme needs a name")?;
    if !valid_name(name) {
        return Err("Theme name must use 1-64 letters, digits, - or _".into());
    }
    let mut theme = Theme::builtin().overlaid(name, &[]);
    let roles = doc
        .get("roles")
        .and_then(|v| v.as_table())
        .ok_or("Theme needs roles")?;
    for (key, value) in roles {
        let role = role_from_name(key).ok_or_else(|| format!("Unknown theme role: {key}"))?;
        let row = value.as_table().ok_or("Role must be a table")?;
        let mut style = theme.style(role);
        if let Some(color) = row.get("color") {
            style.foreground_color =
                Some(parse_color(color.as_str().ok_or("Color must be text")?)?);
        }
        for (name, attr) in FLAGS {
            if let Some(value) = row.get(*name) {
                if value.as_bool().ok_or("Style flags must be true or false")? {
                    style.attributes.set(*attr);
                } else {
                    style.attributes.unset(*attr);
                }
            }
        }
        theme.set_style(role, style);
    }
    Ok(theme)
}

pub fn list() -> Result<(Vec<Theme>, Vec<String>), String> {
    list_directory(&directory()?)
}

fn list_directory(dir: &std::path::Path) -> Result<(Vec<Theme>, Vec<String>), String> {
    let mut themes = builtins();
    let mut warnings = Vec::new();
    if !dir.exists() {
        return Ok((themes, warnings));
    }
    let mut paths = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    paths.sort();
    for path in paths.into_iter().filter(|p| {
        p.extension().is_some_and(|s| s == "toml")
            && p.file_name().is_some_and(|s| s != "active.toml")
    }) {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| decode(&text))
        {
            Ok(theme) => themes.push(theme),
            Err(error) => warnings.push(format!(
                "{}: {error}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    Ok((themes, warnings))
}

pub fn save(theme: &Theme) -> Result<(), String> {
    if !valid_name(&theme.name) {
        return Err("Use 1-64 letters, digits, - or _ for the name".into());
    }
    if theme.name == "active" || builtins().iter().any(|t| t.name == theme.name) {
        return Err("Choose a new name to save a copy of a built-in theme".into());
    }
    write(theme, &format!("{}.toml", theme.name))
}

fn write(theme: &Theme, filename: &str) -> Result<(), String> {
    let dir = directory()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::atomic_fs::atomic_write(&dir.join(filename), encode(theme)?.as_bytes())
        .map_err(|e| e.to_string())
}

pub fn select(theme: &Theme) -> Result<(), String> {
    write(theme, "active.toml")?;
    super::set_active(theme.clone());
    Ok(())
}

pub(super) fn restore() -> (Theme, Vec<String>) {
    let result =
        directory().and_then(
            |dir| match std::fs::read_to_string(dir.join("active.toml")) {
                Ok(text) => decode(&text).map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            },
        );
    match result {
        Ok(theme) => (theme.unwrap_or_else(Theme::builtin), vec![]),
        Err(error) => (Theme::builtin(), vec![format!("Theme: {error}")]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_roundtrip_preserves_colors_and_disabled_bold() {
        let mut theme =
            Theme::builtin().overlaid("mine", &[(Role::InlineCode, Color::AnsiValue(231))]);
        let mut style = theme.style(Role::InlineCode);
        style.attributes.unset(Attribute::Bold);
        style.attributes.set(Attribute::Underlined);
        theme.set_style(Role::InlineCode, style);
        let copy = decode(&encode(&theme).unwrap()).unwrap();
        for role in ALL_ROLES {
            assert_eq!(theme.style(*role), copy.style(*role));
        }
    }
    #[test]
    fn malformed_theme_does_not_hide_valid_saved_themes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.toml"), "broken = [").unwrap();
        let theme = Theme::builtin().overlaid("mine", &[]);
        std::fs::write(dir.path().join("mine.toml"), encode(&theme).unwrap()).unwrap();
        let (themes, warnings) = list_directory(dir.path()).unwrap();
        assert!(themes.iter().any(|t| t.name == "mine"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad.toml"));
    }

    #[test]
    fn rejects_path_names_and_unknown_roles() {
        assert!(!valid_name("../oops"));
        assert!(decode("name='mine'\n[roles.unknown]\ncolor='white'").is_err());
    }
    #[test]
    fn all_builtin_colors_roundtrip() {
        for theme in builtins() {
            for role in ALL_ROLES {
                assert_eq!(
                    parse_color(&color_name(theme.color(*role))),
                    Ok(theme.color(*role))
                );
            }
        }
    }
}
