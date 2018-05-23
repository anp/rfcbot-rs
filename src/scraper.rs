use std::thread::{spawn, JoinHandle};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};

use config::{CONFIG, GH_ORGS};
use github;

pub fn start_scraping() -> JoinHandle<()> {
    // spawn the github scraper in the background
    spawn(|| {
        let sleep_duration = Duration::from_secs(CONFIG.github_interval_mins * 60);
        loop {
            match github::most_recent_update() {
                Ok(gh_most_recent) => scrape_github(gh_most_recent),
                Err(why) => error!("Unable to determine most recent GH update: {:?}", why),
            }
            info!("GitHub scraper sleeping for {} seconds ({} minutes)",
                  sleep_duration.as_secs(),
                  CONFIG.github_interval_mins);
            thread::sleep(sleep_duration);
        }
    })
}

pub fn scrape_github(since: DateTime<Utc>) {
    let mut repos = Vec::new();
    for org in &GH_ORGS {
        repos.extend(ok_or!(github::GH.org_repos(org), why => {
            error!("Unable to retrieve repos for {}: {:?}", org, why);
            return;
        }));
    }

    info!("Scraping github activity since {:?}", since);
    let start_time = Utc::now().naive_utc();
    for repo in repos.iter() {
        match github::ingest_since(&repo, since) {
            Ok(_) => info!("Scraped {} github successfully", repo),
            Err(why) => error!("Unable to scrape github {}: {:?}", repo, why),
        }
    }

    info!("Nuking reactions at github since {:?}", since);
    for repo in repos.iter() {
        match github::nuke_reactions(&repo) {
            Ok(_) => info!("Nuked reactions at {} successfully", repo),
            Err(why) => error!("Unable to nuke reactions {}: {:?}", repo, why),
        }
    }

    ok_or!(github::record_successful_update(start_time), why =>
        error!("Problem recording successful update: {:?}", why));
}
