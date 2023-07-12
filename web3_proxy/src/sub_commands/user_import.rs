use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user};
use glob::glob;
use hashbrown::HashMap;
use migration::sea_orm::ActiveValue::NotSet;
use migration::sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryFilter,
    Set,
};
use std::path::{Path, PathBuf};
use std::{fs::File, io::BufReader};
use tracing::info;

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

        anyhow::ensure!(
            import_dir.exists(),
            "import dir ({}) does not exist!",
            import_dir.to_string_lossy()
        );

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
                    info!(
                        "imported {} users from {} files.",
                        imported_user_count, user_file_count
                    );
                    return Err(e.into());
                }
            }
            user_file_count += 1;
        }

        info!(
            "Imported {} user(s) from {} file(s). {} user(s) mapped.",
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
                    info!(
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
        let mut count = 0;

        // TODO: do this all inside a database transaction?

        // TODO: do this with async things from tokio
        let file = File::open(path)?;
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`
        let us = serde_json::from_reader::<_, Vec<user::Model>>(reader)?;

        for import_u in us.into_iter() {
            // first, check if a user already exists with this address
            if let Some(existing_u) = user::Entity::find()
                .filter(user::Column::Address.eq(import_u.address.clone()))
                .one(db_conn)
                .await?
            {
                user_map.insert(import_u.id, existing_u.id);

                // don't increment count because the user already existed
            } else {
                // user address is not known to the local database. no existing_u
                let import_id = import_u.id;

                let mut new_u = import_u.into_active_model();

                new_u.id = NotSet;

                let new_u = new_u.save(db_conn).await?;

                let new_id = *new_u.id.as_ref();

                user_map.insert(import_id, new_id);

                count += 1;
            }
        }

        Ok(count)
    }

    pub async fn import_rpc_key_file(
        &self,
        db_conn: &DatabaseConnection,
        path: PathBuf,
        user_map: &UserMap,
    ) -> anyhow::Result<u64> {
        let mut count = 0;

        // TODO: do this with async things from tokio
        let file = File::open(path)?;
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`
        let rks = serde_json::from_reader::<_, Vec<rpc_key::Model>>(reader)?;

        for import_rk in rks.into_iter() {
            let mapped_id = *user_map
                .get(&import_rk.user_id)
                .context("user mapping required")?;

            if let Some(existing_rk) = rpc_key::Entity::find()
                .filter(rpc_key::Column::SecretKey.eq(import_rk.secret_key))
                .one(db_conn)
                .await?
            {
                // make sure it belongs to the mapped user
                anyhow::ensure!(existing_rk.user_id == mapped_id, "unexpected user id");

                // the key exists under the expected user. we are good to continue
            } else {
                // user address is not known to the local database. no existing_rk
                let mut new_rk = import_rk.into_active_model();

                new_rk.id = NotSet;
                new_rk.user_id = Set(mapped_id);

                new_rk.save(db_conn).await?;

                count += 1;
            }
        }

        Ok(count)
    }
}
