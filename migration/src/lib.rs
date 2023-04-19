pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20220921_181610_log_reverts;
mod m20220928_015108_concurrency_limits;
mod m20221007_213828_accounting;
mod m20221025_210326_add_chain_id_to_reverts;
mod m20221026_230819_rename_user_keys;
mod m20221027_002407_user_tiers;
mod m20221031_211916_clean_up;
mod m20221101_222349_archive_request;
mod m20221108_200345_save_anon_stats;
mod m20221211_124002_request_method_privacy;
mod m20221213_134158_move_login_into_database;
mod m20230117_191358_admin_table;
mod m20230119_204135_better_free_tier;
mod m20230125_204810_stats_v2;
mod m20230130_124740_read_only_login_logic;
mod m20230130_165144_prepare_admin_imitation_pre_login;
mod m20230215_152254_admin_trail;
mod m20230307_002623_migrate_rpc_accounting_to_rpc_accounting_v2;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20220101_000001_create_table::Migration),
            Box::new(m20220921_181610_log_reverts::Migration),
            Box::new(m20220928_015108_concurrency_limits::Migration),
            Box::new(m20221007_213828_accounting::Migration),
            Box::new(m20221025_210326_add_chain_id_to_reverts::Migration),
            Box::new(m20221026_230819_rename_user_keys::Migration),
            Box::new(m20221027_002407_user_tiers::Migration),
            Box::new(m20221031_211916_clean_up::Migration),
            Box::new(m20221101_222349_archive_request::Migration),
            Box::new(m20221108_200345_save_anon_stats::Migration),
            Box::new(m20221211_124002_request_method_privacy::Migration),
            Box::new(m20221213_134158_move_login_into_database::Migration),
            Box::new(m20230117_191358_admin_table::Migration),
            Box::new(m20230119_204135_better_free_tier::Migration),
            Box::new(m20230125_204810_stats_v2::Migration),
            Box::new(m20230130_124740_read_only_login_logic::Migration),
            Box::new(m20230130_165144_prepare_admin_imitation_pre_login::Migration),
            Box::new(m20230215_152254_admin_trail::Migration),
            Box::new(m20230307_002623_migrate_rpc_accounting_to_rpc_accounting_v2::Migration),
        ]
    }
}
