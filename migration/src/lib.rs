pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20220921_181610_log_reverts;
mod m20220928_015108_concurrency_limits;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20220101_000001_create_table::Migration),
            Box::new(m20220921_181610_log_reverts::Migration),
            Box::new(m20220928_015108_concurrency_limits::Migration),
        ]
    }
}
