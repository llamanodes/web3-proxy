use ethers::prelude::rand::{self, distributions::Alphanumeric, Rng};
use migration::sea_orm::DatabaseConnection;
use std::process::Command as SyncCommand;
use std::time::Duration;
use tokio::{
    net::TcpStream,
    process::Command as AsyncCommand,
    time::{sleep, Instant},
};
use tracing::{info, trace, warn};
use web3_proxy::relational_db::get_migrated_db;

/// on drop, the mysql docker container will be shut down
pub struct TestMysql {
    pub url: Option<String>,
    pub container_name: String,
}

impl TestMysql {
    pub async fn spawn() -> Self {
        // sqlite doesn't seem to work. our migrations are written for mysql
        // so lets use docker to start mysql
        let password: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();

        let random: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();

        let db_container_name = format!("web3-proxy-test-{}", random);

        info!(%db_container_name);

        // create the db_data as soon as the url is known
        // when this is dropped, the db will be stopped
        let mut test_mysql = Self {
            container_name: db_container_name.clone(),
            url: None,
        };

        let _ = AsyncCommand::new("docker")
            .args([
                "run",
                "--name",
                &db_container_name,
                "--rm",
                "-d",
                "-e",
                &format!("MYSQL_ROOT_PASSWORD={}", password),
                "-e",
                "MYSQL_DATABASE=web3_proxy_test",
                "-p",
                "0:3306",
                "mysql",
            ])
            .output()
            .await
            .expect("failed to start db");

        // give the db a second to start
        // TODO: wait until docker says it is healthy
        sleep(Duration::from_secs(1)).await;

        // TODO: why is this always empty?!
        let docker_inspect_output = AsyncCommand::new("docker")
            .args(["inspect", &db_container_name])
            .output()
            .await
            .unwrap();

        let docker_inspect_json = String::from_utf8(docker_inspect_output.stdout).unwrap();

        trace!(%docker_inspect_json);

        let docker_inspect_json: serde_json::Value =
            serde_json::from_str(&docker_inspect_json).unwrap();

        let mysql_ports = docker_inspect_json
            .get(0)
            .unwrap()
            .get("NetworkSettings")
            .unwrap()
            .get("Ports")
            .unwrap()
            .get("3306/tcp")
            .unwrap()
            .get(0)
            .unwrap();

        trace!(?mysql_ports);

        let mysql_port: u64 = mysql_ports
            .get("HostPort")
            .expect("unable to determine mysql port")
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let mysql_ip = mysql_ports
            .get("HostIp")
            .and_then(|x| x.as_str())
            .expect("unable to determine mysql ip");

        let db_url = format!(
            "mysql://root:{}@{}:{}/web3_proxy_test",
            password, mysql_ip, mysql_port
        );

        info!(%db_url, "waiting for start");

        test_mysql.url = Some(db_url.clone());

        let start = Instant::now();
        let max_wait = Duration::from_secs(30);
        loop {
            if start.elapsed() > max_wait {
                panic!("db took too long to start");
            }

            if TcpStream::connect(format!("{}:{}", mysql_ip, mysql_port))
                .await
                .is_ok()
            {
                break;
            };

            // not open wait. sleep and then try again
            sleep(Duration::from_secs(1)).await;
        }

        // TODO: make sure mysql is actually ready for connections
        sleep(Duration::from_secs(1)).await;

        info!(%db_url, elapsed=%start.elapsed().as_secs_f32(), "db post is open. Migrating now...");

        // try to migrate
        let start = Instant::now();
        let max_wait = Duration::from_secs(30);
        loop {
            if start.elapsed() > max_wait {
                panic!("db took too long to start");
            }

            match get_migrated_db(db_url.clone(), 1, 1).await {
                Ok(_) => {
                    // it worked! yey!
                    break;
                }
                Err(err) => {
                    // not connected. sleep and then try again
                    warn!(?err, "unable to migrate db. retrying in 1 second");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }

        info!(%db_url, elapsed=%start.elapsed().as_secs_f32(), "db is migrated");

        test_mysql
    }

    pub async fn conn(&self) -> DatabaseConnection {
        get_migrated_db(self.url.clone().unwrap(), 1, 5)
            .await
            .unwrap()
    }
}

impl Drop for TestMysql {
    fn drop(&mut self) {
        info!(%self.container_name, "killing db");

        let _ = SyncCommand::new("docker")
            .args(["kill", "-s", "9", &self.container_name])
            .output();
    }
}
