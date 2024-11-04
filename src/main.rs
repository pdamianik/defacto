mod client;
mod config;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use anyhow::{anyhow, Context};
use regex::RegexBuilder;
use reqwest_scraper::ScraperResponse;
use subtp::vtt::{VttBlock, WebVtt};
use tracing_subscriber::EnvFilter;
use crate::client::{LoginData, SessionBuilder, TUWElClientBuilder};
use crate::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set default tracing subscriber")?;

    let Config { login } = Config::load("app.toml")?;

    print!("Please enter your TOTP token: ");
    std::io::stdout().flush()?;
    let mut totp = String::new();
    std::io::stdin().read_line(&mut totp)?;

    let session_path = Path::new(".session.json");
    let session = if session_path.exists() {
        let session_file = File::open(session_path)?;
        SessionBuilder::Restore(session_file)
    } else {
        SessionBuilder::New
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

    let session_file = File::create(session_path)?;
    client.persist(&session_file).await?;

    let recordings = client.as_ref().get("https://tuwel.tuwien.ac.at/mod/opencast/view.php?id=2418332")
        .send().await?
        .error_for_status()?
        .xpath().await?;

    let links = recordings.select("/html/body/div[2]/div[4]/div/div/div[2]/div/section/div[2]/div[2]/table/tbody")?
        .as_node()
        .ok_or(anyhow!("Could not find video link in table"))?;
    let links = links
        .findnodes("tr/td/a")?;

    for link in links {
        let video_page = client.as_ref().get(link.attr("href").ok_or(anyhow!("Video link anchor doesn't have href attribute"))?)
            .send().await?
            .error_for_status()?
            .xpath().await?;

        let video_config_script = video_page.select("/html/body/div[2]/div[4]/div/div/div[2]/div/section/div[2]/script")?
            .as_node()
            .ok_or(anyhow!("Could not find video config script tag on video playback site"))?
            .text();

        let video_config_script = video_config_script
            .strip_prefix("//<![CDATA[\n")
            .map(|rest| rest.strip_suffix("//]]>"))
            .flatten()
            .unwrap_or_else(|| {
                tracing::warn!("Failed to remove CDATA wrapper from video config script");
                &video_config_script
            });

        let video_config = video_config_script.strip_prefix("window.episode = ")
            .ok_or(anyhow!("Failed to remove global setter from video config script"))?;
        let video_config = json::parse(video_config)
            .context("Failed to parse config json from video config script")?;
        let captions = &video_config["captions"];
        if let Some(caption_link) = captions[0]["url"].as_str() {
            tracing::info!("{}:", video_config["metadata"]["title"]);
            tracing::debug!(caption_link);

            let captions = client.as_ref().get(caption_link)
                .send().await?
                .text().await?;
            let captions = WebVtt::parse(&captions)
                .context("Failed to parse vtt from caption file")?;

            if captions.blocks.len() == 0 {
                tracing::warn!("Captions are empty");
                continue;
            }

            let raw_transcript = captions.blocks.into_iter()
                .filter_map(|block| if let VttBlock::Que(cue) = block {
                    Some(cue)
                } else {
                    None
                })
                .map(|cue| cue.payload.join(" "))
                .collect::<Vec<_>>();

            let mut transcript = Vec::with_capacity(raw_transcript.len());
            let mut last_block = raw_transcript.first().unwrap().trim().to_string();
            transcript.push(last_block.clone());
            for block in raw_transcript {
                let block = block.trim().to_string();
                if block != last_block {
                    transcript.push(block.clone());
                    last_block = block;
                }
            }

            let transcript = transcript.join(" ");
            tracing::debug!(transcript);

            let patterns = [
                ("De facto", RegexBuilder::new("[^a-zA-Z]de\\s+facto[^a-zA-Z]").case_insensitive(true).build()?),
                ("trivial", RegexBuilder::new("[^a-zA-Z]trivial[^a-zA-Z]").case_insensitive(true).build()?),
                ("Ergibt das Sinn", RegexBuilder::new("[^a-zA-Z]ergibt\\s+das\\s+sinn[^a-zA-Z]").case_insensitive(true).build()?),
            ];

            for (name, pattern) in patterns.iter() {
                let matches = pattern.find_iter(&transcript)
                    .count();
                tracing::info!("Found {matches} {name}s");
            }
        }
    }

    client.persist(&session_file).await?;

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
