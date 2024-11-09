mod client;
mod config;
mod defacto;

use crate::client::{LoginData, SessionBuilder, TUWElClientBuilder};
use crate::config::Config;
use crate::defacto::{DefactoClient, ShortenedDataRow};
use anyhow::Context;
use std::fs::File;
use std::io::Write;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set default tracing subscriber")?;

    let Config { login, cache_path } = Config::load("app.toml")?;
    std::fs::create_dir_all(&cache_path)?;

    print!("Please enter your TOTP token: ");
    std::io::stdout().flush()?;
    let mut totp = String::new();
    std::io::stdin().read_line(&mut totp)?;

    let session_path = cache_path.join(".session.json");
    let session = if session_path.exists() {
        let session_file = File::open(&session_path)?;
        SessionBuilder::Restore(session_file, Some(cache_path.clone()))
    } else {
        SessionBuilder::New(Some(cache_path.clone()))
    };

    let client = TUWElClientBuilder {
        login_data: LoginData {
            username: login.username,
            password: login.password,
            totp: totp.to_string(),
        },
        session,
    }
        .build().await?;
    
    let client = DefactoClient {
        client,
        cache_path: cache_path.clone(),
    };

    let session_file = File::create(&session_path)?;
    client.client.persist(&session_file).await?;

    let data = client.do_stuff().await?;

    client.client.persist(&session_file).await?;
    let mut writer = csv::Writer::from_writer(File::create("results.csv")?);
    let mut shortened_writer = csv::Writer::from_writer(File::create("results.short.csv")?);
    for row in data {
        writer.serialize(row.clone())?;
        let shortened_row: ShortenedDataRow = row.into();
        shortened_writer.serialize(shortened_row)?
    }

    Ok(())

    // let result = get_enrolled_courses_by_timeline_classification::call(
    //     &mut client,
    //     &mut get_enrolled_courses_by_timeline_classification::Params {
    //         classification: Some("all".to_string()),
    //         limit: Some(0),
    //         offset: Some(0),
    //         sort: None,
    //         customfieldname: None,
    //         customfieldvalue: None,
    //         searchvalue: None,
    //     }
    // ).await
    // .unwrap();
    //
    // for course in result.courses.unwrap() {
    //     println!("{}", course.fullname.unwrap())
    // }
}
