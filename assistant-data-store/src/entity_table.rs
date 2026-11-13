//! Single-statement table creation that keeps an entity's unique keys.
use sea_orm::{EntityTrait, Schema, sea_query::TableCreateStatement};

/// `Schema::create_table_from_entity` leaves `unique_key` groups to separate
/// index statements. This keeps them as table constraints in the same
/// statement, so a table created alone still enforces its unique keys.
pub trait EntityTableExt {
    fn entity_table<E: EntityTrait + Copy>(&self, entity: E) -> TableCreateStatement;
}

impl EntityTableExt for Schema {
    fn entity_table<E: EntityTrait + Copy>(&self, entity: E) -> TableCreateStatement {
        let mut table = self.create_table_from_entity(entity);
        for mut index in self.create_index_from_entity(entity) {
            if index.is_unique_key() {
                table.index(&mut index);
            }
        }
        table
    }
}

/// Unique-column attributes that break repeated schema sync. Schema sync
/// keeps only unique keys it can match by column, so a column-level `unique`
/// (a table constraint outside that list) cannot be synchronized twice, and
/// `indexed` on a field with a `unique_key` can take the unique index's name.
/// Attributes of one field are combined; trailing comments are ignored.
/// Test support for entity crates; reads only the given source text.
pub fn unique_attribute_violations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut field: Vec<&str> = Vec::new();
    let mut first_line = 0;
    for (index, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or_default().trim();
        if let Some(args) = code
            .strip_prefix("#[sea_orm(")
            .and_then(|rest| rest.strip_suffix(")]"))
        {
            if field.is_empty() {
                first_line = index + 1;
            }
            let names: Vec<&str> = args
                .split(',')
                .map(|arg| arg.split('=').next().unwrap_or_default().trim())
                .collect();
            if names.contains(&"unique") {
                found.push(format!("{}: {}", index + 1, line.trim()));
            }
            field.extend(names);
        } else if !code.is_empty() && !code.starts_with("#[") {
            if field.contains(&"unique_key") && field.contains(&"indexed") {
                found.push(format!("{first_line}: unique_key with indexed"));
            }
            field.clear();
        }
    }
    found
}

/// Check every entity source in `dir` with [`unique_attribute_violations`].
/// Test support for entity crates.
pub fn entity_dir_violations(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("entity directory") {
        let path = entry.expect("entity entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let source = std::fs::read_to_string(&path).expect("entity source");
            found.extend(
                unique_attribute_violations(&source)
                    .into_iter()
                    .map(|line| format!("{}:{line}", path.display())),
            );
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::agent_schedule;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

    async fn unique_constraints(db: &sea_orm::DatabaseConnection) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS n FROM pragma_index_list('agent_schedule') \
                 WHERE \"unique\" = 1 AND origin = 'u'",
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "n").unwrap()
    }

    #[tokio::test]
    async fn a_table_created_alone_keeps_its_unique_keys() {
        let schema = Schema::new(DbBackend::Sqlite);
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute(&schema.entity_table(agent_schedule::Entity))
            .await
            .unwrap();
        let declared = schema
            .create_index_from_entity(agent_schedule::Entity)
            .iter()
            .filter(|index| index.is_unique_key())
            .count() as i64;
        assert!(declared >= 1);
        assert_eq!(unique_constraints(&db).await, declared);
        let plain = Database::connect("sqlite::memory:").await.unwrap();
        plain
            .execute(&schema.create_table_from_entity(agent_schedule::Entity))
            .await
            .unwrap();
        assert_eq!(unique_constraints(&plain).await, 0);
    }

    #[test]
    fn unique_attributes_flag_column_unique_and_shadowed_keys() {
        let source = "\
            #[sea_orm(unique)]\n    pub a: i64,\n\
            #[sea_orm(unique, nullable)] // trailing\n    pub b: i64,\n\
            #[sea_orm(unique_key = \"c\", indexed)]\n    pub c: i64,\n\
            #[sea_orm(unique_key = \"d\")]\n    #[sea_orm(indexed)]\n    pub d: i64,\n\
            #[sea_orm(unique_key = \"e\")]\n    pub e: i64,\n\
            #[sea_orm(indexed)]\n    pub f: i64,\n\
            /// #[sea_orm(unique)] in a comment\n    pub g: i64,\n";
        let found = unique_attribute_violations(source);
        let lines: Vec<&str> = found
            .iter()
            .map(|item| item.split(':').next().unwrap())
            .collect();
        assert_eq!(lines, ["1", "3", "5", "7"], "{found:?}");
    }

    #[test]
    fn shared_entities_sync_their_unique_keys() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/entity");
        assert_eq!(entity_dir_violations(&dir), Vec::<String>::new());
    }
}
