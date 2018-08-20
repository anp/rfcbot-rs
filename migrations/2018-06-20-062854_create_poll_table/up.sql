CREATE TABLE poll (
    id SERIAL PRIMARY KEY,
    fk_issue INTEGER UNIQUE NOT NULL REFERENCES issue (id),
    fk_initiator INTEGER NOT NULL REFERENCES githubuser (id),
    fk_initiating_comment INTEGER NOT NULL REFERENCES issuecomment (id),
    fk_bot_tracking_comment INTEGER NOT NULL REFERENCES issuecomment (id),
    poll_question VARCHAR NOT NULL,
    poll_created_at TIMESTAMP NOT NULL,
    poll_closed BOOLEAN NOT NULL,
    poll_teams VARCHAR NOT NULL
);

CREATE TABLE poll_response_request (
    id SERIAL PRIMARY KEY,
    fk_poll INTEGER NOT NULL REFERENCES poll (id) ON DELETE CASCADE,
    fk_respondent INTEGER NOT NULL REFERENCES githubuser (id),
    responded BOOLEAN NOT NULL,
    UNIQUE (fk_poll, fk_respondent)
);
