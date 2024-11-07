use std::sync::LazyLock;
use anyhow::{anyhow, Context};
use json::JsonValue;
use regex::{Regex, RegexBuilder};
use reqwest::IntoUrl;
use reqwest_scraper::ScraperResponse;
use serde::{Deserialize, Serialize};
use subtp::vtt::{VttBlock, WebVtt};
use tokio::task;
use crate::client::TUWElClient;

const PATTERNS: [(&'static str, LazyLock<Regex>); 3] = [
    ("De facto", LazyLock::new(|| RegexBuilder::new("[^a-zA-Z]de\\s+facto[^a-zA-Z]").case_insensitive(true).build().unwrap())),
    ("trivial", LazyLock::new(|| RegexBuilder::new("[^a-zA-Z]trivial[^a-zA-Z]").case_insensitive(true).build().unwrap())),
    ("Ergibt das Sinn", LazyLock::new(|| RegexBuilder::new("[^a-zA-Z]ergibt\\s+das\\s+sinn[^a-zA-Z]").case_insensitive(true).build().unwrap())),
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DataRow {
    title: String,
    link: String,
    transcript: String,
    defacto: usize,
    trivial: usize,
    sinn: usize,
}

impl Into<ShortenedDataRow> for DataRow {
    fn into(self) -> ShortenedDataRow {
        ShortenedDataRow {
            title: self.title,
            link: self.link,
            defacto: self.defacto,
            trivial: self.trivial,
            sinn: self.sinn,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ShortenedDataRow {
    title: String,
    link: String,
    defacto: usize,
    trivial: usize,
    sinn: usize,
}

#[derive(Debug, Clone)]
pub struct DefactoClient {
    pub client: TUWElClient,
}

impl DefactoClient {
    pub async fn do_stuff(&self) -> anyhow::Result<Vec<DataRow>> {
        let links = self.get_video_links("https://tuwel.tuwien.ac.at/mod/opencast/view.php?id=2418332").await?;

        tracing::debug!(?links);
        let handles = links.into_iter()
            .map(|link| {
                let client = self.clone();
                task::spawn(async move {
                    client.get_data(link).await
                })
            })
            .collect::<Vec<_>>();
        
        let mut data = Vec::with_capacity(handles.len());
        
        for handle in handles {
            match handle.await? {
                Ok(result) => data.push(result),
                Err(err) => tracing::error!(?err)
            }
        }

        Ok(data)
    }
    
    pub async fn get_data(&self, video_page: impl IntoUrl) -> anyhow::Result<DataRow> {
        let link = video_page.as_str().to_string();
        let video_config = self.get_video_config(video_page).await?;

        let title = video_config["metadata"]["title"].as_str()
            .ok_or(anyhow!("Could not find title in video metadata"))?;
        tracing::info!("{}:", title);

        let transcript = self.get_transcript(&video_config).await?;
        tracing::trace!(transcript);

        let mut counts = [0; 3];
        for (index, (name, pattern)) in PATTERNS.iter().enumerate() {
            let matches = pattern.find_iter(&transcript)
                .count();
            counts[index] = matches;
            tracing::debug!("Found {matches} {name}s");
        }

        Ok(DataRow {
            title: title.to_string(),
            link,
            transcript,
            defacto: counts[0],
            trivial: counts[1],
            sinn: counts[2],
        })
    }

    pub async fn get_video_links(&self, link: impl IntoUrl) -> anyhow::Result<Vec<String>> {
        let recordings = self.client.as_ref().get(link)
            .send().await?
            .error_for_status()?
            .xpath().await?;

        let links = recordings.select("/html/body/div[2]/div[4]/div/div/div[2]/div/section/div[2]/div[2]/table/tbody")?
            .as_node()
            .ok_or(anyhow!("Could not find video link in table"))?;
        let links = links
            .findnodes("tr/td/a")?
            .iter()
            .filter_map(|node| node.attr("href"))
            .collect();

        Ok(links)
    }

    pub async fn get_video_config(&self, link: impl IntoUrl) -> anyhow::Result<JsonValue> {
        let video_page = self.client.as_ref().get(link)
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

        Ok(video_config)
    }
    
    fn get_caption_url(video_config: &JsonValue) -> Option<&str> {
        let captions = if let JsonValue::Array(captions) = &video_config["captions"] {
            captions
        } else {
            return None
        };
        
        let caption = captions.iter()
            .find(|caption| caption["format"].as_str() == Some("vtt") && caption["lang"] == "de")?;
        
        caption["url"].as_str()
    }

    fn get_video_url(video_config: &JsonValue) -> Option<&str> {
        let streams = if let JsonValue::Array(streams) = &video_config["streams"] {
            streams
        } else {
            return None;
        };

        streams.iter()
            .find(|stream| stream["role"].as_str() == Some("mainAudio"))
            .and_then(|stream| {
                let mp4_streams = if let JsonValue::Array(mp4_streams) = &stream["sources"]["mp4"] {
                    mp4_streams
                } else {
                    return None;
                };

                mp4_streams.iter()
                    .filter_map(|stream| {
                        let src = stream["src"].as_str()?;
                        let w = stream["res"]["w"].as_usize()?;
                        let h = stream["res"]["h"].as_usize()?;
                        Some((src, w * h))
                    })
                    .min_by(|(_, size_a), (_, size_b)| size_a.cmp(size_b))
                    .map(|(src, _)| src)
            })
    }

    pub async fn get_transcript(&self, video_config: &JsonValue) -> anyhow::Result<String> {
        let transcript = if let Some(caption_url) = Self::get_caption_url(video_config) {
            self.get_opencast_transcript(caption_url).await
        } else {
            Err(anyhow!("Could not find a caption url"))
        };
        
        match transcript {
            Ok(transcript) => Ok(transcript),
            Err(err) => {
                tracing::warn!("{err:?}");
                
                if let Some(video_url) = Self::get_video_url(video_config) {
                    Ok(self.get_whisper_transcript(video_url).await)
                } else {
                    Err(anyhow!("Could not find a video url"))
                }?
            }
        }
    }

    pub async fn get_opencast_transcript(&self, caption_url: impl IntoUrl) -> anyhow::Result<String> {
        tracing::info!("Downloading captions from: {}", caption_url.as_str());
        let captions = self.client.as_ref().get(caption_url)
            .send().await?
            .text().await?;
        let captions = WebVtt::parse(&captions)
            .context("Failed to parse vtt from caption file")?;

        if captions.blocks.len() == 0 {
            return Err(anyhow!("Captions are empty"))
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

        Ok(transcript.join(" "))
    }
    
    pub async fn get_whisper_transcript(&self, video_url: impl IntoUrl) -> anyhow::Result<String> {
        tracing::info!("Downloading video to parse captions from: {}", video_url.as_str());
        Err(anyhow!("Not implemented"))
    }
}
