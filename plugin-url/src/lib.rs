use google_youtube3::api::{PlaylistListResponse, SearchListResponse, VideoListResponse};
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use async_trait::async_trait;
use irc::proto::{Command, Message};
use nom::{
    bytes::complete::{tag, take_while},
    character::complete::{digit1, multispace0, multispace1},
    combinator::{all_consuming, map, opt},
    multi::separated_list0,
    sequence::{pair, preceded, terminated},
    Finish, IResult,
};
use parking_lot::Mutex;
use plugin_core::{Error, Plugin, Result};
use url::Url;

mod parsing_utils;

#[derive(Deserialize)]
struct YtConfig {
    youtube_api_key: Option<String>,
}

pub struct UrlPlugin {
    seen_urls: Arc<Mutex<HashMap<String, VecDeque<Url>>>>,
    client: reqwest::Client,
    yt_api_key: Option<String>,
}

impl UrlPlugin {
    fn new() -> Result<Self> {
        let path = "golem_config.dhall";
        let yt_config: YtConfig =
            serde_dhall::from_file(path)
                .parse()
                .map_err(|err| Error::Wrapped {
                    source: Box::new(err),
                    ctx: format!("Failed to read config at {path}"),
                })?;
        if yt_config.youtube_api_key.is_some() {
            log::info!("Url plugin initialized with youtube api credentials.");
        } else {
            log::warn!("Url plugin is missing youtube api key.");
        }

        Ok(UrlPlugin {
            seen_urls: Default::default(),
            client: reqwest::Client::new(),
            yt_api_key: yt_config.youtube_api_key,
        })
    }

    fn add_urls(&self, channel: &str, urls: Vec<Url>) {
        let mut seen_urls = self.seen_urls.lock();
        let e = seen_urls.entry(channel.to_string()).or_default();
        for url in urls {
            log::debug!("Adding url to chan: {url}");
            e.push_back(url);
            if e.len() > 10 {
                e.pop_front();
            }
        }
    }

    async fn in_msg(&self, msg: &Message) -> Result<Option<Message>> {
        if let Command::PRIVMSG(source, privmsg) = &msg.command {
            self.add_urls(source, parse_urls(privmsg)?);

            if let Some(cmd) = parse_command(privmsg) {
                let (mb_idx, mb_target) = cmd;
                let channel = match msg.response_target() {
                    None => return Ok(None),
                    Some(target) => target,
                };
                let message = self.get_url(channel, mb_idx.unwrap_or(0)).await?;

                let target = mb_target.map(|t| format!("{t}: ")).unwrap_or_default();
                let msg = format!("{target}{message}");
                return Ok(Some(Command::PRIVMSG(channel.to_string(), msg).into()));
            }
        }
        Ok(None)
    }

    async fn get_url(&self, channel: &str, idx: usize) -> Result<String> {
        let mb_url = {
            let urls_guard = self.seen_urls.lock();
            urls_guard
                .get(channel)
                .and_then(|urls| urls.get(urls.len() - 1 - idx))
                // clone the url so that we can release the lock.
                // This avoid holding it across await points when fetching data for the url
                .cloned()
        };
        let url = match mb_url {
            Some(u) => u,
            None => return Ok(format!("No stored url found at index {idx}")),
        };

        match &self.yt_api_key {
            Some(yt_key) if is_yt_url(&url) => self.get_yt_url(&url, yt_key).await,
            _ => self.get_regular_url(&url).await,
        }
    }

    async fn get_regular_url(&self, url: &Url) -> Result<String> {
        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|err| Error::Wrapped {
                source: Box::new(err),
                ctx: format!("Cannot GET {url}"),
            })?;

        let status_code = resp.status();
        if status_code != reqwest::StatusCode::OK {
            return Ok(format!("Oops, wrong status code, got {}", status_code));
        }

        match resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
        {
            Some(ct) if ct.contains("text") || ct.contains("html") => (),
            Some(ct) => {
                return Ok(format!(
                    "Cannot extract title from content type {ct} for {url}"
                ))
            }
            _ => return Ok(format!("No valid content type found for {url}")),
        };

        let body = resp.text().await.map_err(|err| Error::Wrapped {
            source: Box::new(err),
            ctx: format!("Cannot extract body at {url}"),
        })?;

        let selector = scraper::Selector::parse("title").unwrap();
        if let Some(title) = scraper::Html::parse_document(&body)
            .select(&selector)
            .next()
        {
            let title = title.text().into_iter().collect::<String>();
            Ok(format!("{title} [{url}]"))
        } else {
            Ok(format!("No title found at {url}"))
        }
    }

    async fn get_yt_url(&self, url: &Url, yt_api_key: &str) -> Result<String> {
        let yt_id = match extract_yt_id(url) {
            Some(x) => x,
            None => {
                return Ok(format!(
                    "Ook Ook 🙈, pas possible de trouver quoi query pour {}",
                    url
                ))
            }
        };

        log::debug!("fetching yt data for {yt_id:?}");
        match yt_id {
            YtId::Video(vid_id) => {
                let vids: VideoListResponse =
                    self.yt_api_call(yt_api_key, "videos", &vid_id).await?;
                match vids.items.unwrap_or_default().first() {
                    Some(vid) => {
                        let snip = vid.snippet.as_ref().unwrap();
                        let title = snip.title.as_deref().unwrap_or("");
                        let chan = snip.channel_title.as_deref().unwrap_or("");
                        Ok(format!("{} [{}] [{}]", &title, &chan, &url))
                    }
                    None => Ok(format!("Rien trouvé pour vidéo {vid_id}")),
                }
            }
            YtId::Channel(chan_name) => {
                let raw_resp = self
                    .client
                    .get("https://www.googleapis.com/youtube/v3/search")
                    .query(&[("key", yt_api_key)])
                    .query(&[("part", "snippet")])
                    .query(&[("type", "channel")])
                    .query(&[("q", chan_name)])
                    .send()
                    .await
                    .map_err(|err| Error::Wrapped {
                        source: Box::new(err),
                        ctx: format!("Failed to fetch channel with id {chan_name}"),
                    })?;

                if raw_resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(format!("Pas trouvé de chan pour {chan_name}"));
                }

                if raw_resp.status() != reqwest::StatusCode::OK {
                    return Ok(format!("Ooops, status code: {}", raw_resp.status()));
                }

                let results: SearchListResponse =
                    raw_resp.json().await.map_err(|err| Error::Wrapped {
                        source: Box::new(err),
                        ctx: format!("Cannot parse response when fetching channel {chan_name}"),
                    })?;

                match results.items.unwrap_or_default().first() {
                    Some(search_result) => {
                        let snip = search_result.snippet.as_ref().unwrap();
                        let title = snip.channel_title.as_deref().unwrap_or("");
                        let description = snip.description.as_deref().unwrap_or("");
                        if description.is_empty() {
                            Ok(format!("Channel: {} [{}]", title, url))
                        } else {
                            Ok(format!("Channel: {} ({}) [{}]", title, description, url))
                        }
                    }
                    None => Ok(format!("Pas trouvé de chan pour {chan_name}")),
                }
            }
            YtId::Playlist(playlist_id) => {
                let playlists: PlaylistListResponse = self
                    .yt_api_call(yt_api_key, "playlists", &playlist_id)
                    .await?;
                match playlists.items.unwrap_or_default().first() {
                    Some(playlist) => {
                        let snip = playlist.snippet.as_ref().unwrap();
                        let title = snip.title.as_deref().unwrap_or("");
                        Ok(format!("Playlist: {} [{}]", &title, &url))
                    }
                    None => Ok(format!("Pas de playlist trouvée pour {playlist_id}")),
                }
            }
        }
    }

    async fn yt_api_call<T, Q>(&self, yt_api_key: &str, resource: &str, resource_id: Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: serde::Serialize + std::fmt::Display,
    {
        let mut url = Url::parse("https://www.googleapis.com/youtube/v3").unwrap();
        url.path_segments_mut().unwrap().push(resource);

        self.client
            .get(url)
            .query(&[("id", &resource_id)])
            .query(&[("key", yt_api_key.to_owned())])
            .query(&[("part", "snippet")])
            .send()
            .await
            .and_then(|x| x.error_for_status())
            .map_err(|err| Error::Wrapped {
                source: Box::new(err),
                ctx: format!("Failed to fetch {resource} with id {resource_id}"),
            })?
            .json()
            .await
            .map_err(|err| Error::Wrapped {
                source: Box::new(err),
                ctx: format!("Failed to fetch {resource} with id {resource_id}"),
            })
    }
}

#[async_trait]
impl Plugin for UrlPlugin {
    async fn init() -> Result<Self> {
        UrlPlugin::new()
    }

    fn get_name(&self) -> &'static str {
        "url"
    }

    async fn in_message(&self, msg: &Message) -> Result<Option<Message>> {
        self.in_msg(msg).await
    }
}

fn parse_urls(msg: &str) -> Result<Vec<Url>> {
    match separated_list0(multispace1, parse_url)(msg) {
        Ok((_, urls)) => Ok(urls.into_iter().flatten().collect()),
        Err(_) => Err(plugin_core::Error::Synthetic(format!(
            "Cannot parse url from {msg}"
        ))),
    }
}

fn parse_url(raw: &str) -> IResult<&str, Option<Url>> {
    map(
        take_while(|c: char| !(c == ' ' || c == '\t' || c == '\r' || c == '\n')),
        |word| Url::parse(word).ok(),
    )(raw)
}

/// returns Option<(optional_url_index, optional_target_nick)>
fn parse_command(msg: &str) -> Option<(Option<usize>, Option<&str>)> {
    let cmd = preceded(
        parsing_utils::command_prefix,
        map(
            parsing_utils::with_target(pair(tag("url"), opt(preceded(multispace1, digit1)))),
            |((_, mb_idx), mb_target)| {
                let idx = mb_idx.and_then(|raw| str::parse(raw).ok());
                (idx, mb_target)
            },
        ),
    );
    all_consuming(terminated(cmd, multispace0))(msg)
        .finish()
        .map(|x| x.1)
        .ok()
}

const YT_HOSTNAMES: [&str; 5] = [
    "youtube.com",
    "www.youtube.com",
    "youtu.be",
    "www.youtu.be",
    "m.youtube.com",
];

fn is_yt_url(url: &Url) -> bool {
    url.host()
        .map(|h| match h {
            url::Host::Domain(domain) => YT_HOSTNAMES.contains(&domain),
            url::Host::Ipv4(_) | url::Host::Ipv6(_) => false,
        })
        .unwrap_or(false)
}

#[derive(PartialEq, Eq, Debug)]
enum YtId<'url> {
    Video(Cow<'url, str>),
    Channel(&'url str),
    Playlist(Cow<'url, str>),
}

fn extract_yt_id(url: &Url) -> Option<YtId<'_>> {
    let mut segments = url.path_segments()?;
    let first_segment = segments.next();
    let second_segment = segments.next();

    if matches!(url.host(), Some(url::Host::Domain("youtu.be"))) {
        return first_segment.map(|v| YtId::Video(Cow::Borrowed(v)));
    }

    match first_segment {
        Some("c") | Some("channel") | Some("user") => second_segment.map(YtId::Channel),
        Some("watch") => {
            url.query_pairs()
                .find_map(|(k, v)| if k == "v" { Some(YtId::Video(v)) } else { None })
        }
        Some("shorts") => second_segment.map(|v| YtId::Video(Cow::Borrowed(v))),
        Some("playlist") => url.query_pairs().find_map(|(k, v)| {
            if k == "list" {
                Some(YtId::Playlist(v))
            } else {
                None
            }
        }),
        _ => None,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_simple_url() {
        assert_eq!(
            parse_urls("http://coucou.com").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        )
    }

    #[test]
    fn test_url_prefix() {
        assert_eq!(
            parse_urls("  http://coucou.com").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        );
        assert_eq!(
            parse_urls("some stuff before  http://coucou.com").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        );
    }

    #[test]
    fn test_url_suffix() {
        assert_eq!(
            parse_urls("http://coucou.com some stuff after").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        );
    }

    #[test]
    fn test_url_surround() {
        assert_eq!(
            parse_urls("some stuff before http://coucou.com some stuff after").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        );
    }

    #[test]
    fn test_weird_chars() {
        assert_eq!(
            parse_urls("http://coucou.com	taaaaabs").unwrap(),
            vec![Url::parse("http://coucou.com").unwrap()]
        );
    }

    #[test]
    fn test_multiple_urls() {
        assert_eq!(
            parse_urls("hello http://coucou.com some stuff and https://blah.foo.com to finish")
                .unwrap(),
            vec![
                Url::parse("http://coucou.com").unwrap(),
                Url::parse("https://blah.foo.com").unwrap(),
            ]
        );
    }

    #[test]
    fn test_simple_command_no_match() {
        assert_eq!(parse_command("λlol"), None);
    }

    #[test]
    fn test_simple_command() {
        assert_eq!(parse_command("λurl"), Some((None, None)));
    }

    #[test]
    fn test_command_with_idx() {
        assert_eq!(parse_command("λurl 2"), Some((Some(2), None)));
    }

    #[test]
    fn test_command_with_target() {
        assert_eq!(
            parse_command("λurl > charlie"),
            Some((None, Some("charlie")))
        );
    }

    #[test]
    fn test_command_with_idx_and_target() {
        assert_eq!(
            parse_command("λurl 3 > charlie"),
            Some((Some(3), Some("charlie")))
        );
    }

    #[test]
    fn test_is_yt_url() {
        assert!(!is_yt_url(
            &Url::parse("https://github.com/CoucouInc/rustygolem").unwrap()
        ));

        assert!(is_yt_url(
            &Url::parse("https://youtube.com/c/BosnianApeSociety").unwrap()
        ));

        assert!(is_yt_url(
            &Url::parse("https://www.youtube.com/watch?v=0F5GQAnj0lo").unwrap()
        ));

        assert!(is_yt_url(
            &Url::parse("https://youtu.be/haLBM94SENg?t=256").unwrap()
        ));

        assert!(is_yt_url(
            &Url::parse("https://m.youtube.com/watch?v=haLBM94SENg").unwrap()
        ));

        // https://m.youtube.com/watch?list=PLJcTRymdlUQPwx8qU4ln83huPx-6Y3XxH&v=5MKjPYuD60I&feature=emb_imp_woyt]
    }

    #[test]
    fn test_extract_yt_id() {
        assert_eq!(
            extract_yt_id(&Url::parse("https://github.com/CoucouInc/rustygolem").unwrap()),
            None
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/results?search_query=mj").unwrap()),
            None
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://youtu.be/6gwBOTggfRc").unwrap()),
            Some(YtId::Video("6gwBOTggfRc".into()))
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/watch?v=ZZ3F3zWiEmc").unwrap()),
            Some(YtId::Video("ZZ3F3zWiEmc".into()))
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/shorts/EU4p-OC4O3o").unwrap()),
            Some(YtId::Video("EU4p-OC4O3o".into()))
        );

        assert_eq!(
            extract_yt_id(
                &Url::parse("https://www.youtube.com/c/%E3%81%8B%E3%82%89%E3%82%81%E3%82%8B")
                    .unwrap()
            ),
            // からめる
            Some(YtId::Channel("%E3%81%8B%E3%82%89%E3%82%81%E3%82%8B"))
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/c/inanutshell").unwrap()),
            Some(YtId::Channel("inanutshell"))
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/c/inanutshell/videos").unwrap()),
            Some(YtId::Channel("inanutshell"))
        );

        assert_eq!(
            extract_yt_id(
                &Url::parse("https://www.youtube.com/channel/UCworsKCR-Sx6R6-BnIjS2MA").unwrap()
            ),
            Some(YtId::Channel("UCworsKCR-Sx6R6-BnIjS2MA"))
        );

        assert_eq!(
            extract_yt_id(&Url::parse("https://youtube.com/c/BosnianApeSociety").unwrap()),
            Some(YtId::Channel("BosnianApeSociety"))
        );

        assert_eq!(
            extract_yt_id(
                &Url::parse(
                    "https://www.youtube.com/playlist?list=PLoBxKk9n0UWcv0HTYARFyCb0s9P21cDSd"
                )
                .unwrap()
            ),
            Some(YtId::Playlist("PLoBxKk9n0UWcv0HTYARFyCb0s9P21cDSd".into()))
        );

        //

        assert_eq!(
            extract_yt_id(&Url::parse("https://www.youtube.com/user/VieDeChouhartem").unwrap()),
            Some(YtId::Channel("VieDeChouhartem"))
        );
    }

    // https://youtu.be/6gwBOTggfRc
}
