-- The SQLite schema: the same tables and columns as migrations/postgres
-- (0001 to 0007), in SQLite's types. JSON is text, uuids are 16-byte blobs,
-- times are unix seconds (milliseconds where Postgres' are read as ms).

create table agents (
    id blob primary key,
    parent blob references agents (id),
    spec text not null,
    epoch integer not null default 0,
    created_at integer not null default (unixepoch())
);

create table events (
    agent_id blob not null references agents (id),
    seq integer not null,
    event text not null,
    at integer not null default (unixepoch()),
    primary key (agent_id, seq)
);

create table mail (
    id integer primary key autoincrement,
    addr text not null,
    mail text not null,
    taken boolean not null default false
);

create index mail_pending on mail (addr, id) where not taken;

create table cluster_versions (
    version integer primary key autoincrement,
    files text not null,
    applied_at integer not null default (unixepoch()),
    applied_by text not null
);

create table tokens (
    hash text primary key,
    kind text not null,
    name text not null,
    created_at integer not null default (unixepoch())
);

create index tokens_principal on tokens (kind, name);

create table residents (
    name text primary key,
    agent_id blob not null references agents (id)
);

create table agent_snapshots (
    agent_id blob primary key references agents (id),
    seq integer not null,
    state text not null
);

create table deliveries (
    id integer primary key autoincrement,
    route text not null,
    -- unix milliseconds
    at integer not null default (cast(unixepoch('subsec') * 1000 as integer)),
    payload text not null,
    outcomes text not null
);

create index deliveries_route on deliveries (route, id desc);

create table blobs (
    hash text primary key,
    mime text not null,
    size integer not null,
    data blob not null,
    created_at integer not null default (unixepoch()),
    touched_at integer not null default (unixepoch())
);

create index blobs_touched on blobs (touched_at);

create table hub_leader (
    id integer primary key default 1 check (id = 1),
    term integer not null,
    url text not null,
    since integer not null default (unixepoch())
);

create table holds (
    name text primary key,
    frozen boolean not null,
    -- unix milliseconds
    since integer
);

create table held (
    hold text not null,
    seq integer not null,
    route text not null,
    payload text not null,
    primary key (hold, seq)
);
