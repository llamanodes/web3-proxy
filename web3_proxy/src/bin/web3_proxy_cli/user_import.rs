use argh::FromArgs;
use glob::glob;
use hashbrown::HashMap;
use log::{info, warn};
use migration::sea_orm::DatabaseConnection;
use std::path::{Path, PathBuf};

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Import users from another database.
#[argh(subcommand, name = "user_import")]
pub struct UserImportSubCommand {
    #[argh(positional)]
    export_timestamp: u64,

    #[argh(positional, default = "\"./data/users\".to_string()")]
    /// where to write the file
    /// TODO: validate this is a file here?
    input_dir: String,
}

/// Map ids in the export to ids in our database.
type UserMap = HashMap<u64, u64>;

impl UserImportSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let import_dir = Path::new(&self.input_dir);

        if !import_dir.exists() {
            return Err(anyhow::anyhow!(
                "import dir ({}) does not exist!",
                import_dir.to_string_lossy()
            ));
        }

        let user_glob_path = import_dir.join(format!("{}-users-*.json", self.export_timestamp));

        let user_glob_path = user_glob_path.to_string_lossy();

        info!("Scanning {}", user_glob_path);

        let mut user_map = HashMap::new();
        let mut user_file_count = 0;
        let mut imported_user_count = 0;
        for entry in glob(&user_glob_path)? {
            match entry {
                Ok(path) => {
                    imported_user_count +=
                        self.import_user_file(db_conn, path, &mut user_map).await?
                }
                Err(e) => {
                    warn!(
                        "imported {} users from {} files.",
                        imported_user_count, user_file_count
                    );
                    return Err(e.into());
                }
            }
            user_file_count += 1;
        }

        info!(
            "Imported {} user(s) from {} file(s). {} users mapped.",
            imported_user_count,
            user_file_count,
            user_map.len()
        );

        let rpc_key_glob_path =
            import_dir.join(format!("{}-rpc_keys-*.json", self.export_timestamp));

        let rpc_key_glob_path = rpc_key_glob_path.to_string_lossy();

        info!("Scanning {}", rpc_key_glob_path);

        let mut rpc_key_file_count = 0;
        let mut imported_rpc_key_count = 0;
        for entry in glob(&rpc_key_glob_path)? {
            match entry {
                Ok(path) => {
                    imported_rpc_key_count +=
                        self.import_rpc_key_file(db_conn, path, &user_map).await?
                }
                Err(e) => {
                    warn!(
                        "imported {} users from {} files.",
                        imported_rpc_key_count, rpc_key_file_count
                    );
                    return Err(e.into());
                }
            }
            rpc_key_file_count += 1;
        }

        info!(
            "Imported {} rpc key(s) from {} file(s)",
            imported_rpc_key_count, rpc_key_file_count
        );

        Ok(())
    }

    pub async fn import_user_file(
        &self,
        db_conn: &DatabaseConnection,
        path: PathBuf,
        user_map: &mut UserMap,
    ) -> anyhow::Result<u64> {
        let count = 0;
        // TODO: do this all inside a database transaction?
        // for each file in the path, read as json
        // -- for each entry in the json
        // ---- let user_id = if user is in the database
        // ------ add user to the database
        // ---- else
        // ------ add user to the database
        // ---- add user to the map.
        todo!()
    }

    pub async fn import_rpc_key_file(
        &self,
        db_conn: &DatabaseConnection,
        path: PathBuf,
        user_map: &UserMap,
    ) -> anyhow::Result<u64> {
        let count = 0;
        // TODO: do this all inside a database transaction?
        // for each file in the path, read as json
        // -- for each entry in the json
        // ---- let rpc_key_id = if rpc_key is in the database
        // ------ continue
        // ---- else
        // ------ add rpc_key to the database
        todo!()
    }
}
