//! Which app opens a file: `/mnt/etc/gui/open` (next to the launcher's `apps`), one `extension<TAB>command` per line, `#` comments. The
//! command is run with the file's path as its last argument, like the launcher's commands.

/// The table's (extension, command) pairs; malformed lines are skipped.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .map(|(e, c)| (e.trim().trim_start_matches('.').to_lowercase(), c.trim().to_string()))
        .filter(|(e, c)| !e.is_empty() && !c.is_empty())
        .collect()
}

/// The command for a file named `name`, if its extension has one.
pub fn command<'a>(table: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let ext = crate::entry::extension(name)?;
    table.iter().find(|(e, _)| *e == ext).map(|(_, c)| c.as_str())
}
