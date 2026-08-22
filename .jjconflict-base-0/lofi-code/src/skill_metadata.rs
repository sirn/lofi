pub(crate) fn description(content: &str) -> Option<String> {
    frontmatter_description(content).or_else(|| first_prose_line(content))
}

pub(crate) fn frontmatter_description(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let (frontmatter, _) = rest.split_once("\n---")?;
    frontmatter.lines().find_map(|line| {
        let value = line.trim().strip_prefix("description:")?;
        let value = value.trim().trim_matches(['"', '\'']).trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

pub(crate) fn first_prose_line(content: &str) -> Option<String> {
    let prose = if let Some(rest) = content.strip_prefix("---") {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        rest.split_once("\n---").map_or(content, |(_, prose)| prose)
    } else {
        content
    };
    prose
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_prefers_frontmatter() {
        assert_eq!(
            description(
                "---\nname: test\ndescription: 'Frontmatter description'\n---\n\nBody description\n"
            )
            .as_deref(),
            Some("Frontmatter description")
        );
    }

    #[test]
    fn description_uses_body_prose_when_frontmatter_has_no_description() {
        assert_eq!(
            description("---\nname: test\n---\n\n# Heading\n\nBody description\n").as_deref(),
            Some("Body description")
        );
    }
}
