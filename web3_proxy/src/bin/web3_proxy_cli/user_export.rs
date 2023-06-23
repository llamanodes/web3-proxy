use argh::FromArgs;
use entities::{rpc_key, user};
use migration::sea_orm::{DatabaseConnection, EntityTrait, PaginatorTrait};
use std::fs::{self, create_dir_all};
use std::path::Path;
use tracing::info;

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Export users from the database.
#[argh(subcommand, name = "user_export")]
pub struct UserExportSubCommand {
    /// where to write the file
    /// TODO: validate this is a valid path here?
    #[argh(positional, default = "\"./data/users\".to_string()")]
    output_dir: String,
}

impl UserExportSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        // create the output dir if it does not exist
        create_dir_all(&self.output_dir)?;

        let now = chrono::Utc::now().timestamp();

        let export_dir = Path::new(&self.output_dir);

        // get all the users from the database (paged)
        let mut user_pages = user::Entity::find().paginate(db_conn, 1000);

        // TODO: for now all user_tier tables match in all databases, but in the future we might need to export/import this

        // save all users to a file
        let mut user_file_count = 0;
        while let Some(users) = user_pages.fetch_and_next().await? {
            let export_file = export_dir.join(format!("{}-users-{}.json", now, user_file_count));

            fs::write(
                export_file,
                serde_json::to_string_pretty(&users).expect("users should serialize"),
            )?;

            user_file_count += 1;
        }

        info!(
            "Saved {} user file(s) to {}",
            user_file_count,
            export_dir.to_string_lossy()
        );

        // get all the rpc keys from the database (paged)
        let mut rpc_key_pages = rpc_key::Entity::find().paginate(db_conn, 1000);

        let mut rpc_key_file_count = 0;
        while let Some(rpc_keys) = rpc_key_pages.fetch_and_next().await? {
            let export_file =
                export_dir.join(format!("{}-rpc_keys-{}.json", now, rpc_key_file_count));

            fs::write(
                export_file,
                serde_json::to_string_pretty(&rpc_keys).expect("rpc_keys should serialize"),
            )?;

            rpc_key_file_count += 1;
        }

        info!(
            "Saved {} rpc key file(s) to {}",
            rpc_key_file_count,
            export_dir.to_string_lossy()
        );

        Ok(())
    }
}
