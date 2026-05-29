use rusqlite::Connection;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        hugin::storage::default_db_path()
            .unwrap()
            .display()
            .to_string()
    });
    let c = Connection::open(&path).unwrap();
    let mut stmt = c
        .prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    for row in rows {
        let (name, sql) = row.unwrap();
        println!("{name}:\n  {}\n", sql.replace('\n', "\n  "));
    }
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap();
    println!("entries: {n}");
}
