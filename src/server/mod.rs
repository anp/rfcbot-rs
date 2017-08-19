use iron::prelude::*;
use mount::Mount;

use config::CONFIG;
use github::webhooks;

mod handlers;

pub fn serve() {
    let mut mount = Mount::new();

    mount.mount("/pullrequests/", router!(prs: get "/" => handlers::pull_requests));
    mount.mount("/issues/", router!(issues: get "/" => handlers::issues));
    mount.mount("/nightlies/", router!(releases: get "/" => handlers::nightlies));
    mount.mount("/hot-issues/", router!(hotissues: get "/" => handlers::hot_issues));

    mount.mount("/fcp/",
                router!(
        allfcps: get "/all" => handlers::list_fcps,
        usernamefcps: get "/:username" => handlers::member_nags
    ));

    mount.mount("/github-webhook", router!(ghwebhook: post "/" => webhooks::handler));

    // middleware goes here

    let server_addr = format!("0.0.0.0:{}", CONFIG.server_port);
    info!("Starting API server running at {}", &server_addr);
    Iron::new(mount).http(&*server_addr).unwrap();
}
