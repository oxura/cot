//! Regression coverage for models reached through same-crate reexports.
use std::path::PathBuf;
use std::process::{Command, Output};

use cot_cli::migration_generator::{
    DynDependency, DynOperation, GeneratedMigration, MigrationGenerator, MigrationGeneratorOptions,
    SourceFile,
};
use cot_cli::test_harness::CotProjectBuilder;
use syn::parse_quote;

const PARENT: &str = "use cot::db::{Auto, model}; #[derive(Debug)] #[model] pub struct AParent { #[model(primary_key)] pub id: Auto<i64> }";

fn layout(
    root: &str,
    modules: &str,
    parent_file: &str,
    parent_type: &str,
) -> Vec<(String, String)> {
    vec![
        ("main.rs".into(), root.into()),
        ("models/mod.rs".into(), modules.into()),
        (parent_file.into(), PARENT.into()),
        (
            "models/child.rs".into(),
            format!(
                "use cot::db::{{Auto, ForeignKey, model}}; use {parent_type}; #[derive(Debug)] #[model] pub struct ZChild {{ #[model(primary_key)] pub id: Auto<i64>, pub parent: ForeignKey<AParent> }}"
            ),
        ),
    ]
}

fn glob_layout() -> Vec<(String, String)> {
    layout(
        "mod models; use models::*;",
        "pub mod parent; pub mod child;",
        "models/parent.rs",
        "crate::parent::AParent",
    )
}

fn files(sources: &[(String, String)]) -> Vec<SourceFile> {
    sources
        .iter()
        .map(|(path, source)| SourceFile::parse(PathBuf::from(path), source).unwrap())
        .collect()
}

fn generator() -> MigrationGenerator {
    MigrationGenerator::new(
        PathBuf::from("Cargo.toml"),
        "identity_test".into(),
        MigrationGeneratorOptions::default(),
    )
}

fn generate(sources: &[(String, String)]) -> GeneratedMigration {
    generator()
        .generate_migrations_as_generated_from_files(files(sources))
        .unwrap()
        .unwrap()
}

#[test]
fn root_glob_preserves_dependency_order_and_original_foreign_key_path() {
    let sources = glob_layout();
    for shift in 0..sources.len() {
        let mut reordered = sources.clone();
        reordered.rotate_left(shift);
        let migration = generate(&reordered);
        assert!(
            migration.dependencies.is_empty(),
            "dependencies: {:?}; operations: {:?}",
            migration.dependencies,
            migration.operations
        );
        assert_eq!(migration.operations.len(), 2);
        let DynOperation::CreateModel { table_name, .. } = &migration.operations[0] else {
            panic!("expected parent creation")
        };
        assert_eq!(table_name, "identity_test__a_parent");
        let DynOperation::CreateModel {
            table_name, fields, ..
        } = &migration.operations[1]
        else {
            panic!("expected child creation")
        };
        assert_eq!(table_name, "identity_test__z_child");
        assert_eq!(
            fields[1].foreign_key.as_ref().unwrap().to_model,
            parse_quote!(crate::parent::AParent)
        );
    }
}

#[test]
fn earlier_migration_dependency_keeps_the_accessible_alias() {
    let sources = glob_layout();
    let first = vec![
        sources[0].clone(),
        ("models/mod.rs".into(), "pub mod parent;".into()),
        sources[2].clone(),
    ];
    let first_migration = generator()
        .generate_migrations_as_source_from_files(files(&first))
        .unwrap()
        .unwrap();
    let mut second = sources;
    second.push((
        format!("migrations/{}.rs", first_migration.name),
        first_migration.content,
    ));
    let migration = generate(&second);
    assert_eq!(migration.operations.len(), 1);
    assert_eq!(migration.dependencies.len(), 2);
    assert!(migration.dependencies.contains(&DynDependency::Migration {
        app: "identity_test".into(),
        migration: "m_0001_initial".into()
    }));
    assert!(migration.dependencies.contains(&DynDependency::Model {
        model_type: parse_quote!(crate::parent::AParent)
    }));
}

#[test]
fn alias_cycle_uses_the_same_identity_for_field_splitting() {
    let mut sources = glob_layout();
    sources[2].1 = "use cot::db::{Auto, ForeignKey, model}; use crate::child::ZChild; #[derive(Debug)] #[model] pub struct AParent { #[model(primary_key)] pub id: Auto<i64>, pub child: ForeignKey<ZChild> }".into();
    let migration = generate(&sources);
    assert!(
        migration.dependencies.is_empty(),
        "dependencies: {:?}; operations: {:?}",
        migration.dependencies,
        migration.operations
    );
    assert_eq!(migration.operations.len(), 3);
    assert!(matches!(
        migration.operations[0],
        DynOperation::CreateModel { .. }
    ));
    assert!(matches!(
        migration.operations[1],
        DynOperation::CreateModel { .. }
    ));
    let DynOperation::AddField { field, .. } = &migration.operations[2] else {
        panic!("expected split foreign key")
    };
    assert!(field.foreign_key.is_some());
    // This checks the existing cycle transformation, not AddField's database
    // constraint support. Runtime FK enforcement below covers acyclic models.
}

fn success(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command {command:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

const RUNTIME: &str = r#"
mod migrations;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cot::db::{Database, Model};
    use cot::db::migrations::{MigrationEngine, wrap_migrations};
    use crate::models::child::ZChild as Child;
    use PARENT_PATH as Parent;
    assert_eq!(migrations::MIGRATIONS.len(), 1);
    assert!(migrations::MIGRATIONS[0].dependencies().is_empty());
    let engine = MigrationEngine::new(wrap_migrations(migrations::MIGRATIONS))?;
    let db = Database::new("sqlite::memory:").await?;
    db.raw("PRAGMA foreign_keys = ON").await?;
    engine.run(&db).await?;
    let parent = Parent::TABLE_NAME.as_str();
    let child = Child::TABLE_NAME.as_str();
    db.raw(&format!("INSERT INTO {parent} (id) VALUES (101)")).await?;
    db.raw(&format!("INSERT INTO {child} (id, parent) VALUES (1, 101)")).await?;
    let error = db.raw(&format!("INSERT INTO {child} (id, parent) VALUES (2, 999)")).await.expect_err("orphan FK accepted");
    assert!(match error {
        cot::db::DatabaseError::ForeignKeyViolation => true,
        cot::db::DatabaseError::DatabaseEngineError(error) => error.as_database_error().is_some_and(|error| error.is_foreign_key_violation()),
        _ => false,
    });
    MigrationEngine::new(wrap_migrations(migrations::MIGRATIONS))?.run(&db).await?;
    assert_eq!(db.count(&Parent::objects()).await?, 1);
    assert_eq!(db.count(&Child::objects()).await?, 1);
    db.close().await?;
    Ok(())
}
"#;

fn runtime_case(sources: &[(String, String)], parent_path: &str) {
    let root = format!(
        "{}\n{}",
        sources[0].1,
        RUNTIME.replace("PARENT_PATH", parent_path)
    );
    let mut builder = CotProjectBuilder::new(assert_cmd::cargo::cargo_bin!("cot").to_path_buf())
        .main_rs(root)
        .cargo_toml_extra(
            "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }",
        );
    for (path, source) in &sources[1..] {
        builder = builder.with_file(format!("src/{path}"), source);
    }
    let project = builder.build().unwrap();
    // Compile the genuine module/import tree before running the generator.
    success(&mut project.cargo_cmd("check", &[]));
    success(&mut project.cot_cmd(&["migration", "make", "."]));
    success(&mut project.cargo_cmd("run", &["--quiet"]));
    let module = project.path().join("src/migrations.rs");
    let migration = project.path().join("src/migrations/m_0001_initial.rs");
    let before = (
        std::fs::read(&module).unwrap(),
        std::fs::read(&migration).unwrap(),
    );
    success(&mut project.cot_cmd(&["migration", "make", "."]));
    assert_eq!(
        before,
        (
            std::fs::read(module).unwrap(),
            std::fs::read(migration).unwrap()
        )
    );
    assert_eq!(
        std::fs::read_dir(project.path().join("src/migrations"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires Cargo subprocesses")]
fn generated_root_glob_migration_runs_with_foreign_keys() {
    runtime_case(&glob_layout(), "crate::parent::AParent");
}

#[test]
#[cfg_attr(miri, ignore = "requires Cargo subprocesses")]
fn generated_module_alias_migration_runs_with_foreign_keys() {
    runtime_case(
        &layout(
            "mod models; use models::parent as facade;",
            "pub mod parent; pub mod child;",
            "models/parent.rs",
            "crate::facade::AParent",
        ),
        "crate::facade::AParent",
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires Cargo subprocesses")]
fn generated_struct_reexport_migration_runs_with_foreign_keys() {
    runtime_case(
        &layout(
            "mod models; use models::parent::AParent;",
            "pub mod parent; pub mod child;",
            "models/parent.rs",
            "crate::AParent",
        ),
        "crate::AParent",
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires Cargo subprocesses")]
fn generated_private_implementation_facade_remains_accessible() {
    runtime_case(
        &layout(
            "mod models;",
            "mod implementation; pub use implementation::AParent; pub mod child;",
            "models/implementation.rs",
            "crate::models::AParent",
        ),
        "crate::models::AParent",
    );
}

#[test]
fn adding_an_alias_foreign_key_keeps_owner_and_target_dependencies() {
    let mut first = glob_layout();
    first[3].1 = "use cot::db::{Auto, model}; #[derive(Debug)] #[model] pub struct ZChild { #[model(primary_key)] pub id: Auto<i64> }".into();
    let first_migration = generator()
        .generate_migrations_as_source_from_files(files(&first))
        .unwrap()
        .unwrap();
    let mut second = glob_layout();
    second.push((
        format!("migrations/{}.rs", first_migration.name),
        first_migration.content,
    ));
    let migration = generate(&second);
    assert_eq!(migration.operations.len(), 1);
    assert!(matches!(
        migration.operations[0],
        DynOperation::AddField { .. }
    ));
    assert_eq!(migration.dependencies.len(), 3);
    assert!(migration.dependencies.contains(&DynDependency::Migration {
        app: "identity_test".into(),
        migration: "m_0001_initial".into()
    }));
    for model_type in [
        parse_quote!(crate::models::child::ZChild),
        parse_quote!(crate::parent::AParent),
    ] {
        assert!(
            migration
                .dependencies
                .contains(&DynDependency::Model { model_type })
        );
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires Cargo subprocesses")]
fn alias_foreign_key_to_an_earlier_migration_runs_in_order() {
    let sources = glob_layout();
    let project = CotProjectBuilder::new(assert_cmd::cargo::cargo_bin!("cot").to_path_buf())
        .main_rs("mod models; use models::*; fn main() {}")
        .cargo_toml_extra(
            "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }",
        )
        .with_file("src/models/mod.rs", "pub mod parent;")
        .with_file("src/models/parent.rs", PARENT)
        .build()
        .unwrap();
    success(&mut project.cargo_cmd("check", &[]));
    success(&mut project.cot_cmd(&["migration", "make", "."]));
    let first_path = project.path().join("src/migrations/m_0001_initial.rs");
    let first_bytes = std::fs::read(&first_path).unwrap();
    for (path, source) in &sources[1..] {
        std::fs::write(project.path().join("src").join(path), source).unwrap();
    }
    let runtime = RUNTIME
        .replace("PARENT_PATH", "crate::parent::AParent")
        .replace(
            "assert_eq!(migrations::MIGRATIONS.len(), 1);",
            "assert_eq!(migrations::MIGRATIONS.len(), 2);",
        )
        .replace(
            "assert!(migrations::MIGRATIONS[0].dependencies().is_empty());",
            "assert_eq!(migrations::MIGRATIONS.iter().find(|migration| migration.name().starts_with(\"m_0002_auto_\")).unwrap().dependencies().len(), 2);",
        );
    std::fs::write(
        project.path().join("src/main.rs"),
        format!("{}\n{runtime}", sources[0].1),
    )
    .unwrap();
    success(&mut project.cargo_cmd("check", &[]));
    success(&mut project.cot_cmd(&["migration", "make", "."]));
    success(&mut project.cargo_cmd("run", &["--quiet"]));
    assert_eq!(std::fs::read(first_path).unwrap(), first_bytes);
    let second_path = std::fs::read_dir(project.path().join("src/migrations"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("m_0002_auto_")
        })
        .unwrap();
    let second_bytes = std::fs::read(&second_path).unwrap();
    success(&mut project.cot_cmd(&["migration", "make", "."]));
    assert_eq!(std::fs::read(second_path).unwrap(), second_bytes);
    assert_eq!(
        std::fs::read_dir(project.path().join("src/migrations"))
            .unwrap()
            .count(),
        2
    );
}

// Exercise the unchanged real engine separately from generation. These are
// explicit invalid migration inputs, not substitutes for generated app tests.
mod runtime_dependency_contract {
    use cot::db::Identifier;
    use cot::db::migrations::{DynMigration, MigrationDependency, MigrationEngine, Operation};

    #[derive(Debug)]
    struct Input {
        app: &'static str,
        name: &'static str,
        dependencies: Vec<MigrationDependency>,
        operations: Vec<Operation>,
    }

    impl DynMigration for Input {
        fn app_name(&self) -> &str {
            self.app
        }
        fn name(&self) -> &str {
            self.name
        }
        fn dependencies(&self) -> &[MigrationDependency] {
            &self.dependencies
        }
        fn operations(&self) -> &[Operation] {
            &self.operations
        }
    }

    fn input(
        app: &'static str,
        name: &'static str,
        dependency: Option<MigrationDependency>,
        table: Option<&'static str>,
    ) -> Input {
        Input {
            app,
            name,
            dependencies: dependency.into_iter().collect(),
            operations: table
                .into_iter()
                .map(|table| {
                    Operation::create_model()
                        .table_name(Identifier::new(table))
                        .fields(&[])
                        .build()
                })
                .collect(),
        }
    }

    fn error(inputs: Vec<Input>, expected: &str) {
        let error = MigrationEngine::new(inputs).expect_err("invalid dependency input accepted");
        assert!(
            format!("{error:?}")
                .starts_with(&format!("MigrationError(MigrationSortError({expected}")),
            "wrong error: {error:?}"
        );
    }

    #[test]
    fn named_and_model_cycles_remain_errors() {
        error(
            vec![input(
                "app",
                "self",
                Some(MigrationDependency::migration("app", "self")),
                None,
            )],
            "CycleDetected",
        );
        error(
            vec![input(
                "app",
                "self",
                Some(MigrationDependency::model("app", Identifier::new("parent"))),
                Some("parent"),
            )],
            "CycleDetected",
        );
        error(
            vec![
                input(
                    "app",
                    "first",
                    Some(MigrationDependency::migration("app", "second")),
                    None,
                ),
                input(
                    "app",
                    "second",
                    Some(MigrationDependency::migration("app", "first")),
                    None,
                ),
            ],
            "CycleDetected",
        );
        error(
            vec![
                input(
                    "app",
                    "first",
                    Some(MigrationDependency::model("app", Identifier::new("second"))),
                    Some("first"),
                ),
                input(
                    "app",
                    "second",
                    Some(MigrationDependency::model("app", Identifier::new("first"))),
                    Some("second"),
                ),
            ],
            "CycleDetected",
        );
    }

    #[test]
    fn missing_dependencies_and_cross_app_lookup_remain_strict() {
        error(
            vec![input(
                "app",
                "one",
                Some(MigrationDependency::migration("app", "missing")),
                None,
            )],
            "InvalidDependency",
        );
        error(
            vec![input(
                "app",
                "one",
                Some(MigrationDependency::model(
                    "app",
                    Identifier::new("missing"),
                )),
                None,
            )],
            "InvalidDependency",
        );
        error(
            vec![
                input("owner", "create", None, Some("parent")),
                input(
                    "child",
                    "create",
                    Some(MigrationDependency::model(
                        "wrong_app",
                        Identifier::new("parent"),
                    )),
                    None,
                ),
            ],
            "InvalidDependency",
        );
        assert!(
            MigrationEngine::new(vec![
                input(
                    "child",
                    "create",
                    Some(MigrationDependency::model(
                        "owner",
                        Identifier::new("parent")
                    )),
                    None
                ),
                input("owner", "create", None, Some("parent")),
            ])
            .is_ok()
        );
    }

    #[test]
    fn duplicate_migrations_and_models_remain_errors() {
        error(
            vec![
                input("app", "same", None, None),
                input("app", "same", None, None),
            ],
            "DuplicateMigration",
        );
        error(
            vec![
                input("app", "one", None, Some("parent")),
                input("app", "two", None, Some("parent")),
            ],
            "DuplicateModel",
        );
    }
}
