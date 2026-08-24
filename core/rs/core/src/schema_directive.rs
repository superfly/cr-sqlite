extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{Connection, ResultCode};

/// Read the `skip_hash` directive from the table's CREATE TABLE SQL in sqlite_master.
/// Looks for `/* crsql: skip_hash=1 */` (or `skip_hash=true`, `skip_hash=on`).
/// Returns:
///   Ok(Some(true)) if directive present and enabled,
///   Ok(Some(false)) if directive present and explicitly disabled,
///   Ok(None) if directive absent or on parse error.
pub fn read_skip_hash_directive_opt(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<Option<bool>, ResultCode> {
    let directives = read_directives(db, table)?;
    Ok(directives.get("skip_hash").map(|v| is_truthy(v)))
}

/// Read all crsql directives from the table's CREATE TABLE SQL.
/// Parses `/* crsql: key1=value1, key2=value2, ... */` comments.
fn read_directives(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<alloc::collections::BTreeMap<String, String>, ResultCode> {
    let sql = format!(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?\0"
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_text(1, table, sqlite::Destructor::TRANSIENT)?;
    if stmt.step()? != ResultCode::ROW {
        return Ok(alloc::collections::BTreeMap::new());
    }
    let create_sql = match stmt.column_text(0) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok(alloc::collections::BTreeMap::new()),
    };
    Ok(parse_directives(&create_sql))
}

/// Parse `/* crsql: key=value, ... */` directives from a CREATE TABLE SQL string.
/// Returns a map of key→value pairs.
pub fn parse_directives(create_sql: &str) -> alloc::collections::BTreeMap<String, String> {
    let mut result = alloc::collections::BTreeMap::new();

    // Find all block comments and look for the crsql: prefix
    let mut search_pos = 0;
    while let Some(comment_start) = create_sql[search_pos..].find("/*") {
        let abs_start = search_pos + comment_start;
        if let Some(comment_end_rel) = create_sql[abs_start..].find("*/") {
            let abs_end = abs_start + comment_end_rel + 2;
            let comment_body = &create_sql[abs_start + 2..abs_start + comment_end_rel];

            // Check for crsql: prefix (case-insensitive)
            let trimmed = comment_body.trim();
            if trimmed.to_lowercase().starts_with("crsql:") {
                let directive_str = &trimmed[6..]; // skip "crsql:"
                for pair in directive_str.split(',') {
                    let pair = pair.trim();
                    if let Some(eq_pos) = pair.find('=') {
                        let key = pair[..eq_pos].trim().to_lowercase();
                        let value = pair[eq_pos + 1..].trim().to_string();
                        result.insert(key, value);
                    }
                }
            }
            search_pos = abs_end;
        } else {
            break; // unterminated comment
        }
    }

    result
}

/// Check if a directive value is "truthy": 1, true, on, yes (case-insensitive).
fn is_truthy(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower == "1" || lower == "true" || lower == "on" || lower == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_directives_basic() {
        let sql = "CREATE TABLE foo /* crsql: skip_hash=1, use_rowid_key=0 */ (id INTEGER PRIMARY KEY, x TEXT)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"1".to_string()));
        assert_eq!(directives.get("use_rowid_key"), Some(&"0".to_string()));
    }

    #[test]
    fn test_parse_directives_case_insensitive_prefix() {
        let sql = "CREATE TABLE foo /* CRSQL: skip_hash=1 */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"1".to_string()));
    }

    #[test]
    fn test_parse_directives_no_directive() {
        let sql = "CREATE TABLE foo (id INTEGER PRIMARY KEY, x TEXT)";
        let directives = parse_directives(sql);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_parse_directives_non_crsql_comment() {
        let sql = "CREATE TABLE foo /* just a comment */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_parse_directives_multiple_comments() {
        let sql = "CREATE TABLE foo /* some comment */ /* crsql: skip_hash=1 */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"1".to_string()));
    }

    #[test]
    fn test_parse_directives_true_value() {
        let sql = "CREATE TABLE foo /* crsql: skip_hash=true */ (id BLOB PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"true".to_string()));
        assert!(is_truthy(directives.get("skip_hash").unwrap()));
    }

    #[test]
    fn test_parse_directives_false_value() {
        let sql = "CREATE TABLE foo /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"0".to_string()));
        assert!(!is_truthy(directives.get("skip_hash").unwrap()));
    }

    #[test]
    fn test_parse_directives_empty_value() {
        let sql = "CREATE TABLE foo /* crsql: skip_hash= */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        // Empty value — key exists but value is empty, not truthy
        assert_eq!(directives.get("skip_hash"), Some(&"".to_string()));
        assert!(!is_truthy(directives.get("skip_hash").unwrap()));
    }

    #[test]
    fn test_parse_directives_with_spaces() {
        let sql = "CREATE TABLE foo /* crsql:  skip_hash = 1  ,  use_rowid_key = 1  */ (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert_eq!(directives.get("skip_hash"), Some(&"1".to_string()));
        assert_eq!(directives.get("use_rowid_key"), Some(&"1".to_string()));
    }

    #[test]
    fn test_parse_directives_unterminated_comment() {
        let sql = "CREATE TABLE foo /* crsql: skip_hash=1 (id INTEGER PRIMARY KEY)";
        let directives = parse_directives(sql);
        assert!(directives.is_empty()); // unterminated → no directives
    }

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy("on"));
        assert!(is_truthy("yes"));
        assert!(is_truthy("On"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
        assert!(!is_truthy("random"));
    }
}
