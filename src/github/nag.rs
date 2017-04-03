use chrono::{Duration, UTC};
use diesel::prelude::*;
use diesel;

use config::RFC_BOT_MENTION;
use DB_POOL;
use domain::github::{GitHubUser, Issue, IssueComment, Membership, Team};
use domain::rfcbot::{FcpConcern, FcpProposal, FcpReviewRequest, FeedbackRequest, NewFcpProposal,
                     NewFcpConcern, NewFcpReviewRequest, NewFeedbackRequest};
use domain::schema::*;
use error::*;
use github::models::CommentFromJson;
use super::GH;

// TODO check if new subteam label added for existing proposals

pub fn update_nags(comment: &IssueComment) -> DashResult<()> {
    let conn = &*DB_POOL.get()?;

    let issue = issue::table.find(comment.fk_issue).first::<Issue>(conn)?;

    let author = githubuser::table.find(comment.fk_user).first::<GitHubUser>(conn)?;

    let subteam_members = subteam_members(&issue)?;

    // attempt to parse a command out of the comment
    if let Ok(command) = RfcBotCommand::from_str(&comment.body) {

        // don't accept bot commands from non-subteam members
        if subteam_members.iter().find(|&u| u == &author).is_none() {
            info!("command author ({}) doesn't appear in any relevant subteams",
                  author.login);
            return Ok(());
        }

        debug!("processing rfcbot command: {:?}", &command);
        match command.process(&author, &issue, comment, &subteam_members) {
            Ok(_) => (),
            Err(why) => {
                error!("Unable to process command for comment id {}: {:?}",
                       comment.id,
                       why);
                return Ok(());
            }
        };

        debug!("rfcbot command is processed");

    } else {
        match resolve_applicable_feedback_requests(&author, &issue, comment) {
            Ok(_) => (),
            Err(why) => {
                error!("Unable to resolve feedback requests for comment id {}: {:?}",
                       comment.id,
                       why);
            }
        };
    }

    match evaluate_nags() {
        Ok(_) => (),
        Err(why) => {
            error!("Unable to evaluate outstanding proposals: {:?}", why);
        }
    };

    Ok(())
}

fn update_proposal_review_status(proposal_id: i32) -> DashResult<()> {
    let conn = &*DB_POOL.get()?;
    // this is an updated comment from the bot itself

    // parse out each "reviewed" status for each user, then update them

    let proposal: FcpProposal = fcp_proposal::table.find(proposal_id).first(conn)?;

    // don't update any statuses if the fcp is running or closed
    if proposal.fcp_start.is_some() || proposal.fcp_closed {
        return Ok(());
    }

    let comment: IssueComment = issuecomment::table.find(proposal.fk_bot_tracking_comment)
        .first(conn)?;

    // parse the status comment and mark any new reviews as reviewed
    let reviewed = comment.body
        .lines()
        .filter_map(|line| {
            if line.starts_with("* [") {
                let l = line.trim_left_matches("* [");
                let reviewed = l.starts_with('x');
                let remaining = l.trim_left_matches("x] @").trim_left_matches(" ] @");

                if let Some(username) = remaining.split_whitespace().next() {
                    trace!("reviewer parsed as reviewed? {} (line: \"{}\")",
                           reviewed,
                           l);

                    if reviewed { Some(username) } else { None }
                } else {
                    warn!("An empty usename showed up in comment {} for proposal {}",
                          comment.id,
                          proposal.id);
                    None
                }
            } else {
                None
            }
        });

    for username in reviewed {
        let user: GitHubUser = githubuser::table.filter(githubuser::login.eq(username))
            .first(conn)?;

        {
            use domain::schema::fcp_review_request::dsl::*;
            let mut review_request: FcpReviewRequest =
                fcp_review_request.filter(fk_proposal.eq(proposal.id))
                    .filter(fk_reviewer.eq(user.id))
                    .first(conn)?;

            review_request.reviewed = true;
            diesel::update(fcp_review_request.find(review_request.id)).set(&review_request)
                .execute(conn)?;
        }
    }

    Ok(())
}

fn evaluate_nags() -> DashResult<()> {
    use diesel::prelude::*;
    use domain::schema::fcp_proposal::dsl::*;
    use domain::schema::issuecomment::dsl::*;
    use domain::schema::issuecomment::dsl::id as issuecomment_id;
    let conn = &*DB_POOL.get()?;

    // first process all "pending" proposals (unreviewed or remaining concerns)
    let pending_proposals = match fcp_proposal.filter(fcp_start.is_null())
        .load::<FcpProposal>(conn) {
        Ok(p) => p,
        Err(why) => {
            error!("Unable to retrieve list of pending proposals: {:?}", why);
            return Err(why.into());
        }
    };

    for mut proposal in pending_proposals {
        let initiator = match githubuser::table.find(proposal.fk_initiator)
            .first::<GitHubUser>(conn) {
            Ok(i) => i,
            Err(why) => {
                error!("Unable to retrieve proposal initiator for proposal id {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };

        let issue = match issue::table.find(proposal.fk_issue).first::<Issue>(conn) {
            Ok(i) => i,
            Err(why) => {
                error!("Unable to retrieve issue for proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };

        // if the issue has been closed before an FCP starts,
        // then we just need to cancel the FCP entirely
        if !issue.open {
            match cancel_fcp(&initiator, &issue, &proposal) {
                Ok(_) => (),
                Err(why) => {
                    error!("Unable to cancel FCP for proposal {}: {:?}",
                           proposal.id,
                           why);
                    continue;
                }
            };
        }

        // check to see if any checkboxes were modified before we end up replacing the comment
        match update_proposal_review_status(proposal.id) {
            Ok(_) => (),
            Err(why) => {
                error!("Unable to update review status for proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        }

        // get associated concerns and reviews
        let reviews = match list_review_requests(proposal.id) {
            Ok(r) => r,
            Err(why) => {
                error!("Unable to retrieve review requests for proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };

        let concerns = match list_concerns_with_authors(proposal.id) {
            Ok(c) => c,
            Err(why) => {
                error!("Unable to retrieve concerns for proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };

        let num_active_reviews = reviews.iter().filter(|&&(_, ref r)| !r.reviewed).count();
        let num_active_concerns =
            concerns.iter().filter(|&&(_, ref c)| c.fk_resolved_comment.is_none()).count();

        // update existing status comment with reviews & concerns
        let status_comment = RfcBotComment::new(&issue, CommentType::FcpProposed(
                    &initiator,
                    FcpDisposition::from_str(&proposal.disposition)?,
                    &reviews,
                    &concerns));

        let previous_comment: IssueComment =
            issuecomment.filter(issuecomment_id.eq(proposal.fk_bot_tracking_comment)).first(conn)?;

        if previous_comment.body != status_comment.body {
            // if the comment body in the database equals the new one we generated, then no change
            // is needed from github (this assumes our DB accurately reflects GH's, which should
            // be true in most cases by the time this is called)
            match status_comment.post(Some(proposal.fk_bot_tracking_comment)) {
                Ok(_) => (),
                Err(why) => {
                    error!("Unable to update status comment for proposal {}: {:?}",
                           proposal.id,
                           why);
                    continue;
                }
            };
        }

        if num_active_reviews == 0 && num_active_concerns == 0 {
            // TODO only record the fcp as started if we know that we successfully commented
            // i.e. either the comment claims to have posted, or we get a comment back to reconcile

            // FCP can start now -- update the database
            proposal.fcp_start = Some(UTC::now().naive_utc());
            match diesel::update(fcp_proposal.find(proposal.id)).set(&proposal).execute(conn) {
                Ok(_) => (),
                Err(why) => {
                    error!("Unable to mark FCP {} as started: {:?}", proposal.id, why);
                    continue;
                }
            }

            // attempt to add the final-comment-period label
            // TODO only add label if FCP > 1 day
            use config::CONFIG;
            if CONFIG.post_comments {
                let label_res =
                    GH.add_label(&issue.repository, issue.number, "final-comment-period");

                let added_label = match label_res {
                    Ok(()) => true,
                    Err(why) => {
                        warn!("Unable to add FCP label to {}#{}: {:?}",
                              &issue.repository,
                              issue.number,
                              why);
                        false
                    }
                };

                let comment_type = CommentType::FcpAllReviewedNoConcerns {
                    added_label: added_label,
                    author: &initiator,
                    status_comment_id: proposal.fk_bot_tracking_comment,
                };

                // leave a comment for FCP start
                let fcp_start_comment = RfcBotComment::new(&issue, comment_type);
                match fcp_start_comment.post(None) {
                    Ok(_) => (),
                    Err(why) => {
                        error!("Unable to post comment for FCP {}'s start: {:?}",
                               proposal.id,
                               why);
                        continue;
                    }
                };
            }
        }
    }

    // look for any FCP proposals that entered FCP a week or more ago but aren't marked as closed
    let one_business_week_ago = UTC::now().naive_utc() - Duration::days(10);
    let finished_fcps = match fcp_proposal.filter(fcp_start.le(one_business_week_ago))
        .filter(fcp_closed.eq(false))
        .load::<FcpProposal>(conn) {
        Ok(f) => f,
        Err(why) => {
            error!("Unable to retrieve FCPs that need to be marked as finished: {:?}",
                   why);
            return Err(why.into());
        }
    };

    for mut proposal in finished_fcps {

        let issue = match issue::table.find(proposal.fk_issue).first::<Issue>(conn) {
            Ok(i) => i,
            Err(why) => {
                error!("Unable to find issue to match proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };

        // TODO only update the db if the comment posts, but reconcile if we find out it worked

        // update the fcp
        proposal.fcp_closed = true;
        match diesel::update(fcp_proposal.find(proposal.id)).set(&proposal).execute(conn) {
            Ok(_) => (),
            Err(why) => {
                error!("Unable to update FCP {}: {:?}", proposal.id, why);
                continue;
            }
        }

        let fcp_close_comment = RfcBotComment::new(&issue, CommentType::FcpWeekPassed);
        match fcp_close_comment.post(None) {
            Ok(_) => (),
            Err(why) => {
                error!("Unable to post FCP-ending comment for proposal {}: {:?}",
                       proposal.id,
                       why);
                continue;
            }
        };
    }

    Ok(())
}

fn list_review_requests(proposal_id: i32) -> DashResult<Vec<(GitHubUser, FcpReviewRequest)>> {
    use domain::schema::{fcp_review_request, githubuser};

    let conn = &*DB_POOL.get()?;

    let reviews = fcp_review_request::table.filter(fcp_review_request::fk_proposal.eq(proposal_id))
        .load::<FcpReviewRequest>(conn)?;

    let mut w_reviewers = Vec::with_capacity(reviews.len());

    for review in reviews {
        let initiator = githubuser::table.filter(githubuser::id.eq(review.fk_reviewer))
            .first::<GitHubUser>(conn)?;

        w_reviewers.push((initiator, review));
    }

    w_reviewers.sort_by(|a, b| a.0.login.cmp(&b.0.login));

    Ok(w_reviewers)
}

fn list_concerns_with_authors(proposal_id: i32) -> DashResult<Vec<(GitHubUser, FcpConcern)>> {
    use domain::schema::{fcp_concern, githubuser};

    let conn = &*DB_POOL.get()?;

    let concerns = fcp_concern::table.filter(fcp_concern::fk_proposal.eq(proposal_id))
        .order(fcp_concern::name)
        .load::<FcpConcern>(conn)?;

    let mut w_authors = Vec::with_capacity(concerns.len());

    for concern in concerns {
        let initiator = githubuser::table.filter(githubuser::id.eq(concern.fk_initiator))
            .first::<GitHubUser>(conn)?;

        w_authors.push((initiator, concern));
    }

    Ok(w_authors)
}

fn resolve_applicable_feedback_requests(author: &GitHubUser,
                                        issue: &Issue,
                                        comment: &IssueComment)
                                        -> DashResult<()> {

    use domain::schema::rfc_feedback_request::dsl::*;
    let conn = &*DB_POOL.get()?;

    // check for an open feedback request, close since no longer applicable
    let existing_request = rfc_feedback_request.filter(fk_requested.eq(author.id))
        .filter(fk_issue.eq(issue.id))
        .first::<FeedbackRequest>(conn)
        .optional()?;

    if let Some(mut request) = existing_request {
        request.fk_feedback_comment = Some(comment.id);
        diesel::update(rfc_feedback_request.find(request.id)).set(&request).execute(conn)?;
    }

    Ok(())
}

/// Check if an issue comment is written by a member of one of the subteams labelled on the issue.
fn subteam_members(issue: &Issue) -> DashResult<Vec<GitHubUser>> {
    use diesel::pg::expression::dsl::any;
    use domain::schema::{teams, memberships, githubuser};

    let conn = &*DB_POOL.get()?;

    // retrieve all of the teams tagged on this issue
    let team = teams::table.filter(teams::label.eq(any(&issue.labels))).load::<Team>(conn)?;

    let team_ids = team.into_iter().map(|t| t.id).collect::<Vec<_>>();

    // get all the members of those teams
    let members = memberships::table.filter(memberships::fk_team.eq(any(team_ids)))
        .load::<Membership>(conn)?;

    let member_ids = members.into_iter().map(|m| m.fk_member).collect::<Vec<_>>();

    // resolve each member into an actual user
    let users = githubuser::table.filter(githubuser::id.eq(any(member_ids)))
        .order(githubuser::login)
        .load::<GitHubUser>(conn)?;

    Ok(users)
}

fn cancel_fcp(author: &GitHubUser, issue: &Issue, existing: &FcpProposal) -> DashResult<()> {
    use domain::schema::fcp_proposal::dsl::*;

    let conn = &*DB_POOL.get()?;

    // if exists delete FCP with associated concerns, reviews, feedback requests
    // db schema has ON DELETE CASCADE
    diesel::delete(fcp_proposal.filter(id.eq(existing.id))).execute(conn)?;

    // leave github comment stating that FCP proposal cancelled
    let comment = RfcBotComment::new(issue, CommentType::FcpProposalCancelled(author));
    let _ = comment.post(None);

    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub enum RfcBotCommand<'a> {
    FcpPropose(FcpDisposition),
    FcpCancel,
    Reviewed,
    NewConcern(&'a str),
    ResolveConcern(&'a str),
    FeedbackRequest(&'a str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FcpDisposition {
    Merge,
    Close,
    Postpone,
}

impl FcpDisposition {
    pub fn repr(self) -> &'static str {
        match self {
            FcpDisposition::Merge => "merge",
            FcpDisposition::Close => "close",
            FcpDisposition::Postpone => "postpone",
        }
    }

    pub fn from_str(string: &str) -> DashResult<Self> {
        match string {
            "merge" => Ok(FcpDisposition::Merge),
            "close" => Ok(FcpDisposition::Close),
            "postpone" => Ok(FcpDisposition::Postpone),
            _ => Err(DashError::Misc(None)),
        }
    }
}

impl<'a> RfcBotCommand<'a> {
    pub fn process(self,
                   author: &GitHubUser,
                   issue: &Issue,
                   comment: &IssueComment,
                   issue_subteam_members: &[GitHubUser])
                   -> DashResult<()> {

        let conn = &*DB_POOL.get()?;

        // check for existing FCP
        let existing_proposal = {
            use domain::schema::fcp_proposal::dsl::*;

            fcp_proposal.filter(fk_issue.eq(issue.id))
                .first::<FcpProposal>(conn)
                .optional()?
        };

        match self {
            RfcBotCommand::FcpPropose(disp) => {
                debug!("processing fcp proposal: {:?}", disp);
                use domain::schema::fcp_proposal::dsl::*;
                use domain::schema::{fcp_review_request, issuecomment};

                if existing_proposal.is_none() {
                    // if not exists, create new FCP proposal
                    info!("proposal is a new FCP, creating...");

                    // leave github comment stating that FCP is proposed, ping reviewers
                    let gh_comment =
                        RfcBotComment::new(issue, CommentType::FcpProposed(author, disp, &[], &[]));

                    let gh_comment = gh_comment.post(None)?;
                    info!("Posted base comment to github, no reviewers listed yet");

                    // at this point our new comment doesn't yet exist in the database, so
                    // we need to insert it
                    let gh_comment = gh_comment.with_repo(&issue.repository)?;
                    match diesel::insert(&gh_comment).into(issuecomment::table).execute(conn) {
                        Ok(_) => {},
                        Err(why) => {
                            warn!("Had an issue inserting the new record, probably received a webhook for it: {:?}", why);
                        }
                    }

                    let proposal = NewFcpProposal {
                        fk_issue: issue.id,
                        fk_initiator: author.id,
                        fk_initiating_comment: comment.id,
                        disposition: disp.repr(),
                        fk_bot_tracking_comment: gh_comment.id,
                        fcp_start: None,
                        fcp_closed: false,
                    };

                    let proposal = diesel::insert(&proposal).into(fcp_proposal)
                        .get_result::<FcpProposal>(conn)?;

                    debug!("proposal inserted into the database");

                    // generate review requests for all relevant subteam members

                    let review_requests = issue_subteam_members.iter()
                        .map(|member| {
                            // let's assume the initiator has reviewed it
                            NewFcpReviewRequest {
                                fk_proposal: proposal.id,
                                fk_reviewer: member.id,
                                reviewed: member.id == author.id,
                            }
                        })
                        .collect::<Vec<_>>();

                    diesel::insert(&review_requests).into(fcp_review_request::table)
                        .execute(conn)?;

                    // they're in the database, but now we need them paired with githubuser

                    let review_requests = list_review_requests(proposal.id)?;

                    debug!("review requests inserted into the database");

                    // we have all of the review requests, generate a new comment and post it

                    let new_gh_comment =
                        RfcBotComment::new(issue,
                                           CommentType::FcpProposed(author,
                                                                    disp,
                                                                    &review_requests,
                                                                    &[]));

                    new_gh_comment.post(Some(gh_comment.id))?;

                    debug!("github comment updated with reviewers");
                }
            }
            RfcBotCommand::FcpCancel => {
                if let Some(existing) = existing_proposal {
                    cancel_fcp(author, issue, &existing)?;
                }
            }
            RfcBotCommand::Reviewed => {
                // set a reviewed entry for the comment author on this issue

                use domain::schema::fcp_review_request::dsl::*;

                if let Some(proposal) = existing_proposal {

                    let review_request = fcp_review_request.filter(fk_proposal.eq(proposal.id))
                        .filter(fk_reviewer.eq(author.id))
                        .first::<FcpReviewRequest>(conn)
                        .optional()?;

                    if let Some(mut review_request) = review_request {
                        // store an FK to the comment marking for review (not null fk here means
                        // reviewed)
                        review_request.reviewed = true;

                        diesel::update(fcp_review_request.find(review_request.id))
                            .set(&review_request)
                            .execute(conn)?;
                    }

                }
            }
            RfcBotCommand::NewConcern(concern_name) => {

                if let Some(proposal) = existing_proposal {
                    // check for existing concern
                    use domain::schema::fcp_concern::dsl::*;

                    let existing_concern = fcp_concern.filter(fk_proposal.eq(proposal.id))
                        .filter(name.eq(concern_name))
                        .first::<FcpConcern>(conn)
                        .optional()?;

                    if existing_concern.is_none() {
                        // if not exists, create new concern with this author as creator

                        let new_concern = NewFcpConcern {
                            fk_proposal: proposal.id,
                            fk_initiator: author.id,
                            fk_resolved_comment: None,
                            name: concern_name,
                            fk_initiating_comment: comment.id,
                        };

                        diesel::insert(&new_concern).into(fcp_concern).execute(conn)?;
                    }

                }
            }
            RfcBotCommand::ResolveConcern(concern_name) => {

                debug!("Command is to resolve a concern ({}).", concern_name);

                if let Some(proposal) = existing_proposal {
                    // check for existing concern
                    use domain::schema::fcp_concern::dsl::*;

                    let existing_concern = fcp_concern.filter(fk_proposal.eq(proposal.id))
                        .filter(fk_initiator.eq(author.id))
                        .filter(name.eq(concern_name))
                        .first::<FcpConcern>(conn)
                        .optional()?;

                    if let Some(mut concern) = existing_concern {

                        debug!("Found a matching concern ({})", concern_name);

                        // mark concern as resolved by adding resolved_comment
                        concern.fk_resolved_comment = Some(comment.id);

                        diesel::update(fcp_concern.find(concern.id)).set(&concern)
                            .execute(conn)?;
                    }

                }
            }
            RfcBotCommand::FeedbackRequest(username) => {

                use domain::schema::githubuser;
                use domain::schema::rfc_feedback_request::dsl::*;

                // we'll just assume that this user exists...it's very unlikely that someone
                // will request feedback from a user who's *never* commented or committed
                // on/to a rust-lang* repo
                let requested_user = githubuser::table.filter(githubuser::login.eq(username))
                    .first::<GitHubUser>(conn)?;

                // check for existing feedback request
                let existing_request =
                    rfc_feedback_request.filter(fk_requested.eq(requested_user.id))
                        .filter(fk_issue.eq(issue.id))
                        .first::<FeedbackRequest>(conn)
                        .optional()?;

                if existing_request.is_none() {
                    // create feedback request

                    let new_request = NewFeedbackRequest {
                        fk_initiator: author.id,
                        fk_requested: requested_user.id,
                        fk_issue: issue.id,
                        fk_feedback_comment: None,
                    };

                    diesel::insert(&new_request).into(rfc_feedback_request).execute(conn)?;
                }
            }
        }

        Ok(())
    }

    pub fn from_str(command: &'a str) -> DashResult<RfcBotCommand<'a>> {

        // get the tokens for the command line (starts with a bot mention)
        let command = command.lines()
            .find(|&l| l.starts_with(RFC_BOT_MENTION))
            .ok_or(DashError::Misc(None))?
            .trim_left_matches(RFC_BOT_MENTION)
            .trim_left_matches(':')
            .trim();

        let mut tokens = command.split_whitespace();

        let invocation = tokens.next().ok_or(DashError::Misc(None))?;

        match invocation {
            "fcp" | "pr" => {
                let subcommand = tokens.next().ok_or(DashError::Misc(None))?;

                debug!("Parsed command as new FCP proposal");

                match subcommand {
                    "merge" => Ok(RfcBotCommand::FcpPropose(FcpDisposition::Merge)),
                    "close" => Ok(RfcBotCommand::FcpPropose(FcpDisposition::Close)),
                    "postpone" => Ok(RfcBotCommand::FcpPropose(FcpDisposition::Postpone)),
                    "cancel" => Ok(RfcBotCommand::FcpCancel),
                    _ => {
                        error!("unrecognized subcommand for fcp: {}", subcommand);
                        Err(DashError::Misc(Some(format!("found bad subcommand: {}", subcommand))))
                    }
                }
            }
            "concern" => {

                let name_start = command.find("concern").unwrap() + "concern".len();

                debug!("Parsed command as NewConcern");

                Ok(RfcBotCommand::NewConcern(command[name_start..].trim()))
            }
            "resolved" => {
                // TODO handle "resolve" as well, with the correct tokenization

                let name_start = command.find("resolved").unwrap() + "resolved".len();

                debug!("Parsed command as ResolveConcern");

                Ok(RfcBotCommand::ResolveConcern(command[name_start..].trim()))

            }
            "reviewed" => Ok(RfcBotCommand::Reviewed),
            "f?" => {

                let user = tokens.next()
                    .ok_or_else(|| DashError::Misc(Some("no user specified".to_string())))?;

                if user.is_empty() {
                    return Err(DashError::Misc(Some("no user specified".to_string())));
                }

                Ok(RfcBotCommand::FeedbackRequest(&user[1..]))
            }
            _ => Err(DashError::Misc(None)),
        }
    }
}

struct RfcBotComment<'a> {
    issue: &'a Issue,
    body: String,
}

enum CommentType<'a> {
    FcpProposed(&'a GitHubUser,
                FcpDisposition,
                &'a [(GitHubUser, FcpReviewRequest)],
                &'a [(GitHubUser, FcpConcern)]),
    FcpProposalCancelled(&'a GitHubUser),
    FcpAllReviewedNoConcerns {
        author: &'a GitHubUser,
        status_comment_id: i32,
        added_label: bool,
    },
    FcpWeekPassed,
}

impl<'a> RfcBotComment<'a> {
    fn new(issue: &'a Issue, comment_type: CommentType<'a>) -> RfcBotComment<'a> {

        let body = Self::format(issue, &comment_type);

        RfcBotComment {
            issue: issue,
            body: body,
        }
    }

    fn format(issue: &Issue, comment_type: &CommentType) -> String {

        match *comment_type {
            CommentType::FcpProposed(initiator, disposition, reviewers, concerns) => {
                let mut msg = String::from("Team member @");
                msg.push_str(&initiator.login);
                msg.push_str(" has proposed to ");
                msg.push_str(disposition.repr());
                msg.push_str(" this. The next step is review by the rest of the tagged ");
                msg.push_str("teams:\n\n");

                for &(ref member, ref review_request) in reviewers {

                    if review_request.reviewed {
                        msg.push_str("* [x] @");
                    } else {
                        msg.push_str("* [ ] @");
                    }

                    msg.push_str(&member.login);
                    msg.push('\n');
                }

                if concerns.is_empty() {
                    msg.push_str("\nNo concerns currently listed.\n");
                } else {
                    msg.push_str("\nConcerns:\n\n");
                }

                for &(_, ref concern) in concerns {

                    if let Some(resolved_comment_id) = concern.fk_resolved_comment {
                        msg.push_str("* ~~");
                        msg.push_str(&concern.name);
                        msg.push_str("~~ resolved by ");
                        Self::add_comment_url(issue, &mut msg, resolved_comment_id);
                        msg.push_str("\n");

                    } else {
                        msg.push_str("* ");
                        msg.push_str(&concern.name);
                        msg.push_str(" (");
                        Self::add_comment_url(issue, &mut msg, concern.fk_initiating_comment);
                        msg.push_str(")\n");
                    }
                }

                msg.push_str("\nOnce these reviewers reach consensus, this will enter its final ");
                msg.push_str("comment period. If you spot a major issue that hasn't been raised ");
                msg.push_str("at any point in this process, please speak up!\n");

                msg.push_str("\nSee [this document](");
                msg.push_str("https://github.com/dikaiosune/rust-dashboard/blob/master/RFCBOT.md");
                msg.push_str(") for info about what commands tagged team members can give me.");

                msg
            }

            CommentType::FcpProposalCancelled(initiator) => {
                format!("@{} proposal cancelled.", initiator.login)
            }

            CommentType::FcpAllReviewedNoConcerns { author, status_comment_id, added_label } => {
                let mut msg = String::new();

                msg.push_str(":bell: **This is now entering its final comment period**, ");
                msg.push_str("as per the [review above](");
                Self::add_comment_url(issue, &mut msg, status_comment_id);
                msg.push_str("). :bell:");

                if !added_label {
                    msg.push_str("\n\n*psst @");
                    msg.push_str(&author.login);
                    msg.push_str(", I wasn't able to add the `final-comment-period` label,");
                    msg.push_str(" please do so.*");
                }

                msg
            }

            CommentType::FcpWeekPassed => "The final comment period is now complete.".to_string(),
        }
    }

    fn add_comment_url(issue: &Issue, msg: &mut String, comment_id: i32) {
        let to_add = format!("https://github.com/{}/issues/{}#issuecomment-{}",
                             issue.repository,
                             issue.number,
                             comment_id);
        msg.push_str(&to_add);
    }

    fn post(&self, existing_comment: Option<i32>) -> DashResult<CommentFromJson> {
        use config::CONFIG;

        if CONFIG.post_comments {

            if self.issue.open {
                Ok(match existing_comment {
                    Some(comment_id) => {
                        GH.edit_comment(&self.issue.repository, comment_id, &self.body)
                    }
                    None => GH.new_comment(&self.issue.repository, self.issue.number, &self.body),
                }?)
            } else {
                info!("Skipping comment to {}#{}, the issue is no longer open",
                      self.issue.repository,
                      self.issue.number);

                Err(DashError::Misc(None))
            }

        } else {
            info!("Skipping comment to {}#{}, comment posts are disabled.",
                  self.issue.repository,
                  self.issue.number);
            Err(DashError::Misc(None))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn success_fcp_reviewed() {
        let body = "@rfcbot: reviewed";
        let body_no_colon = "@rfcbot reviewed";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::Reviewed);
    }

    #[test]
    fn success_fcp_merge() {
        let body = "@rfcbot: fcp merge\n\nSome justification here.";
        let body_no_colon = "@rfcbot fcp merge\n\nSome justification here.";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::FcpPropose(FcpDisposition::Merge));
    }

    #[test]
    fn success_fcp_close() {
        let body = "@rfcbot: fcp close\n\nSome justification here.";
        let body_no_colon = "@rfcbot fcp close\n\nSome justification here.";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::FcpPropose(FcpDisposition::Close));
    }

    #[test]
    fn success_fcp_postpone() {
        let body = "@rfcbot: fcp postpone\n\nSome justification here.";
        let body_no_colon = "@rfcbot fcp postpone\n\nSome justification here.";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon,
                   RfcBotCommand::FcpPropose(FcpDisposition::Postpone));
    }

    #[test]
    fn success_fcp_cancel() {
        let body = "@rfcbot: fcp cancel\n\nSome justification here.";
        let body_no_colon = "@rfcbot fcp cancel\n\nSome justification here.";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::FcpCancel);
    }

    #[test]
    fn success_concern() {
        let body = "@rfcbot: concern CONCERN_NAME
someothertext
somemoretext

somemoretext";
        let body_no_colon = "@rfcbot concern CONCERN_NAME
someothertext
somemoretext

somemoretext";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::NewConcern("CONCERN_NAME"));
    }

    #[test]
    fn success_resolve() {
        let body = "@rfcbot: resolved CONCERN_NAME
someothertext
somemoretext

somemoretext";
        let body_no_colon = "@rfcbot resolved CONCERN_NAME
someothertext
somemoretext

somemoretext";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::ResolveConcern("CONCERN_NAME"));
    }

    #[test]
    fn success_resolve_mid_body() {
        let body = "someothertext
@rfcbot: resolved CONCERN_NAME
somemoretext

somemoretext";
        let body_no_colon = "someothertext
somemoretext

@rfcbot resolved CONCERN_NAME

somemoretext";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::ResolveConcern("CONCERN_NAME"));
    }

    #[test]
    fn success_feedback() {
        let body = "@rfcbot: f? @bob
someothertext
somemoretext

somemoretext";
        let body_no_colon = "@rfcbot f? @bob
someothertext
somemoretext

somemoretext";

        let with_colon = RfcBotCommand::from_str(body).unwrap();
        let without_colon = RfcBotCommand::from_str(body_no_colon).unwrap();

        assert_eq!(with_colon, without_colon);
        assert_eq!(with_colon, RfcBotCommand::FeedbackRequest("bob"));
    }
}
