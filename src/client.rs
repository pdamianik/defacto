use anyhow::{anyhow, Context};
use reqwest::{Client, ClientBuilder, Url};
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};
use reqwest_scraper::ScraperResponse;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::sync::{Arc, LazyLock};

const BASE_URL: LazyLock<Url> = LazyLock::new(|| "https://tuwel.tuwien.ac.at/".parse().unwrap());

#[derive(Debug)]
pub enum SessionBuilder {
    New,
    Restore(File),
}

impl SessionBuilder {
    pub async fn build(self, login_data: &LoginData) -> anyhow::Result<Session> {
        match self {
            Self::New => {
                let mut session = Session::default();
                session.login(&login_data).await?;
                Ok(session)
            }
            Self::Restore(file) => {
                Ok(Session::restore(&file, login_data).await?)
            }
        }
    }
}

#[derive(Debug)]
pub struct TUWElClientBuilder {
    pub login_data: LoginData,
    pub session: SessionBuilder,
}

impl TUWElClientBuilder {
    pub async fn build(self) -> anyhow::Result<TUWElClient> {
        let session = self.session.build(&self.login_data).await?;
        Ok(TUWElClient {
            session
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginData {
    pub username: String,
    pub password: String,
    pub totp: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    client: Client,
    cookie_jar: Arc<CookieStoreMutex>,
    session_key: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        let cookie_jar = Arc::new(CookieStoreMutex::new(CookieStore::default()));

        let client = ClientBuilder::new()
            .cookie_store(true)
            .cookie_provider(cookie_jar.clone())
            .build().unwrap();

        Self {
            client,
            cookie_jar,
            session_key: None,
        }
    }
}

impl Session {
    pub async fn restore(file: &File, login_data: &LoginData) -> anyhow::Result<Self> {
        let cookie_jar = CookieStore::load_json(BufReader::new(file)).unwrap(); // TODO: fix conversion to anyhow::Result
        let cookie_jar = Arc::new(CookieStoreMutex::new(cookie_jar));

        let client = ClientBuilder::new()
            .cookie_store(true)
            .cookie_provider(cookie_jar.clone())
            .build()?;

        let mut session = Self {
            client,
            cookie_jar,
            session_key: None,
        };

        if !session.check().await? {
            session.login(login_data).await?;
        } else {
            session.load_key().await?;
        }

        Ok(session)
    }

    pub async fn persist(&self, file: &File) -> anyhow::Result<()> {
        let cookie_jar = self.cookie_jar.lock().unwrap();
        cookie_jar.save_incl_expired_and_nonpersistent_json(&mut BufWriter::new(file)).unwrap(); // TODO: complain
        Ok(())
    }

    pub async fn check(&mut self) -> anyhow::Result<bool> {
        let home_url = BASE_URL.join("/my/").unwrap();
        let response = self.client.get(home_url.clone())
            .send().await.context("Failed to send request to home page")?
            .error_for_status().context("Failed to send request to home page")?;

        Ok(*response.url() == home_url)
    }

    async fn login(&mut self, login_data: &LoginData) -> anyhow::Result<()> {
        let LoginData { username, password, totp } = login_data;
        let url = BASE_URL.join("/auth/saml2/login.php")?;
        let response = self.client.get(url).send().await?;
        let full_url = response.url().clone();

        let html = response.css_selector().await?;
        const AUTH_STATE_INPUT_NAME: &str = "AuthState";
        let auth_state_input = html.select(&format!("form[name=f] input[name={AUTH_STATE_INPUT_NAME}]"))?;
        let auth_state_input = auth_state_input
            .first().ok_or(anyhow!("auth state input not found on TU simple saml login page"))?;
        let auth_state = auth_state_input.attr("value")
            .ok_or(anyhow!("No value attribute found for auth state input"))?;

        let params = [
            ("username", username.as_ref()),
            ("password", password.as_ref()),
            ("totp", totp.as_ref()),
            (AUTH_STATE_INPUT_NAME, auth_state),
        ];

        let mut request_url = full_url.clone();
        request_url.set_query(None);
        let origin = format!("{}://{}", full_url.host().unwrap(), full_url.scheme());
        let request = self.client.post(request_url)
            .header("Origin", origin)
            .header("Referer", full_url.to_string())
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-User", "?1")
            .form(&params);
        let response = request.send().await?;
        let html = response.css_selector().await?;
        let title = html.select("title")?
            .first().ok_or(anyhow!("Failed to find login form response title"))?
            .text();

        match title.as_str() {
            "TU Wien Login" => {
                let error_message = html.select(".message-box.error")?;
                let error_message = error_message.first().ok_or(anyhow!("Failed to find error message in login form response"))?;
                return Err(anyhow!(error_message.inner_html()));
            }
            "Sende Nachricht" => (),
            _ => return Err(anyhow!("Unexpected login form response title {title}"))
        }

        let post_form = html.select("form[method=post]")
            .map_err(|_| anyhow!("Could not find message in login form response"))?;
        let post_form = post_form.first().unwrap();

        let message_data = {
            let mut message_data = HashMap::new();
            let data_inputs = post_form.select("input")?;
            for data_input in data_inputs.iter() {
                if let (Some(name), Some(value)) = (data_input.attr("name"), data_input.attr("value")) {
                    message_data.insert(name.to_string(), value.to_string());
                }
            }
            message_data
        };


        let url = post_form.attr("action")
            .ok_or(anyhow!("Could not extract message action from login form response"))?;
        let _ = self.client.post(url)
            .form(&message_data)
            .send()
            .await?
            .error_for_status()?;

        self.load_key().await
    }

    pub async fn load_key(&mut self) -> anyhow::Result<()> {
        let home_url = BASE_URL.join("/my/").unwrap();
        let response = self.client.get(home_url)
            .send().await.context("Failed to send request to home page")?
            .error_for_status().context("Failed to send request to home page")?;

        let xpath = response.xpath().await?;

        let script = xpath.select("/html/head/script[3]").context("Failed to find session config script")?
            .as_node().ok_or(anyhow!("Failed to find moodle config script"))?;
        let moodle_config = script.text();
        let (moodle_config, _) = moodle_config
            .lines()
            .find(|line| line.starts_with("M.cfg = "))
            .ok_or(anyhow!("Failed to find moodle config setter in moodle config script"))?
            .strip_prefix("M.cfg = ").unwrap()
            .split_once(";").ok_or(anyhow!("Failed to extract moodle config"))?;

        let moodle_config = json::parse(moodle_config).context("Failed to parse moodle config json")?;
        let session_key = moodle_config["sesskey"].as_str()
            .ok_or(anyhow!("Failed to get sesskey from moodle config json"))?;
        self.session_key = Some(session_key.to_string());
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TUWElClient {
    session: Session,
}

impl TUWElClient {
    pub async fn persist(&self, file: &File) -> anyhow::Result<()> {
        self.session.persist(file).await
    }
}

impl AsRef<Client> for TUWElClient {
    fn as_ref(&self) -> &Client {
        &self.session.client
    }
}

// #[derive(Serialize, Deserialize)]
// struct TUWElParam<T: Serialize> {
//     pub args: T,
//     pub index: usize,
//     pub methodname: String,
// }

// impl MoodleClient for TUWElClient {
//     async fn get(&self, func: &str) -> anyhow::Result<Value> {
//         let session_key = self.session.session_key.clone()
//             .ok_or(anyhow!("Session key is not set"))?;
//         let url = {
//             let mut url = BASE_URL.join("/lib/ajax/service.php")?;
//             url.set_query(Some(&format!("sesskey={}&info={func}", session_key)));
//             url
//         };
//         let response = self.session.client.get(url).send().await?;
//         let json = response.json().await?;
//         Ok(json)
//     }
// 
//     async fn post<T: serde::ser::Serialize + ?Sized>(&self, func: &str, params: &T) -> anyhow::Result<serde_json::value::Value> {
//         let session_key = self.session.session_key.clone()
//             .ok_or(anyhow!("Session key is not set"))?;
//         let url = {
//             let mut url = BASE_URL.join("/lib/ajax/service.php")?;
//             url.set_query(Some(&format!("sesskey={}&info={func}", session_key)));
//             url
//         };
//         let params = vec![
//             TUWElParam {
//                 args: params,
//                 index: 0,
//                 methodname: func.to_string(),
//             }
//         ];
//         let response = self.session.client.post(url).json(&params).send().await?;
//         let json = response.json().await?;
//         if let Value::Array(array) = json {
//             let response = array.first()
//                 .ok_or(anyhow!("Received 0 responses"))?;
// 
//             if let Value::Object(object) = response {
//                 let error = object.get("error")
//                     .ok_or(anyhow!("Invalid response format"))?;
// 
//                 match error {
//                     Value::Bool(error) if !error => {
//                         Ok(object.get("data")
//                             .ok_or(anyhow!("Invalid response format"))?.clone())
//                     }
//                     _ => Err(anyhow!("Moodle Error: {error}")),
//                 }
//             } else {
//                 Err(anyhow!("Invalid response format"))
//             }
//         } else {
//             Err(anyhow!("Invalid response format"))
//         }
//     }
// }
