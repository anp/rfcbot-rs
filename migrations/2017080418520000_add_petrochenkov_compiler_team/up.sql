INSERT INTO memberships (fk_member, fk_team)
SELECT u.id, t.id
FROM githubuser u, teams t
WHERE t.ping = 'rust-lang/compiler' AND
    u.login = 'petrochenkov';
