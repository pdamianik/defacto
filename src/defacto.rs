use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use anyhow::{anyhow, Context};
use ffmpeg_next::{channel_layout, format::input, util::{media::Type, frame::Audio}};
use ffmpeg_next::format::{sample, Sample};
use json::JsonValue;
use regex::{Regex, RegexBuilder};
use reqwest::IntoUrl;
use reqwest_scraper::ScraperResponse;
use serde::{Deserialize, Serialize};
use subtp::vtt::{VttBlock, WebVtt};
use tokio::task;
use tracing::{span, Instrument, Level};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
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

#[derive(Debug, Copy, Clone)]
struct STTContext;

impl STTContext {
    const CONTEXT: LazyLock<WhisperContext> = LazyLock::new(|| {
        ffmpeg_next::init().unwrap();

        whisper_rs::install_whisper_tracing_trampoline();
        let model_path = std::env::var("WHISPER_MODEL").unwrap();
        WhisperContext::new_with_params(
            &model_path,
            WhisperContextParameters::default()
        ).unwrap()
        
    });
    
    async fn get_whisper_transcript(path: impl AsRef<Path>) -> anyhow::Result<String> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("de"));
        params.set_translate(false);

        let audio_data = Self::get_audio_data(path)?;

        let mut state = Self::CONTEXT.create_state()?;
        state.full(params, &audio_data[..])?;

        let mut result = String::new();
        let num_segments = state
            .full_n_segments()
            .expect("failed to get number of segments");
        for i in 0..num_segments {
            let segment = state
                .full_get_segment_text(i)
                .expect("failed to get segment");
            result.push_str(&segment);
            let start_timestamp = state
                .full_get_segment_t0(i)
                .expect("failed to get segment start timestamp");
            let end_timestamp = state
                .full_get_segment_t1(i)
                .expect("failed to get segment end timestamp");
            tracing::trace!("[{} - {}]: {}", start_timestamp, end_timestamp, segment);
        }
        
        Ok(result)
    }

    fn get_audio_data(path: impl AsRef<Path>) -> anyhow::Result<Vec<f32>> {
        let mut ictx = input(&path)?;
        let input = ictx
            .streams()
            .best(Type::Audio)
            .ok_or(ffmpeg_next::Error::StreamNotFound)?;
        let stream_index = input.index();

        let context_decoder = ffmpeg_next::codec::context::Context::from_parameters(input.parameters())?;
        let mut decoder = context_decoder.decoder().audio()?;
        let mut resampler = decoder.resampler(
            Sample::F32(sample::Type::Planar),
            channel_layout::ChannelLayout::MONO,
            16_000
        )?;

        let mut data = vec![];

        for (stream, packet) in ictx.packets() {
            if stream.index() == stream_index {
                decoder.send_packet(&packet)?;
                let mut decoded = Audio::empty();
                while decoder.receive_frame(&mut decoded).is_ok() {
                    let mut resampled = Audio::empty();
                    resampler.run(&decoded, &mut resampled)?;
                    data.extend_from_slice(resampled.plane(0));
                }
            }
        }

        Ok(data)
    }
}

#[derive(Debug, Clone)]
pub struct DefactoClient {
    pub client: TUWElClient,
    pub cache_path: PathBuf,
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
        tracing::info!(link, "Getting video config");
        let video_config = self.get_video_config(video_page).await?;

        let title = video_config["metadata"]["title"].as_str()
            .ok_or(anyhow!("Could not find title in video metadata"))?;
        let span = span!(Level::INFO, "video", title);

        async {
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
            .instrument(span)
            .await
    }

    pub async fn get_video_links(&self, link: impl IntoUrl) -> anyhow::Result<Vec<String>> {
        let recordings = self.client.get(link)
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
        let video_page = self.client.get(link)
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
                tracing::warn!("{err}");
                
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
        let captions = self.client.get(caption_url)
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
        let video_url = video_url.into_url()?;
        tracing::info!("Downloading video to parse captions from: {}", &video_url);
        let video_path = {
            let video_path = self.cache_path.join(
                Path::new(video_url.path())
                    .file_name()
                    .ok_or(anyhow!("No video file name"))?
                    .to_owned());
            let mut video_file = File::create(&video_path)?;
            
            let response = self.client.get(video_url)
                .send().await?;
            video_file.write(&response.bytes().await?)?;
            video_path
        };
        
        let transcript = STTContext::get_whisper_transcript(video_path).await?;
        
        Ok(transcript)
    }
}
