-- Applied cluster files; the newest version is the desired state.
create table cluster_versions (
    version bigserial primary key,
    files jsonb not null, -- [{"name": ..., "text": ...}]
    applied_at timestamptz not null default now(),
    applied_by text not null
);

-- API/node tokens; only their SHA-256 is stored.
create table tokens (
    hash text primary key,
    kind text not null, -- user | client | node
    name text not null,
    created_at timestamptz not null default now()
);

create index tokens_principal on tokens (kind, name);

-- Long-lived named agents from the cluster file.
create table residents (
    name text primary key,
    agent_id uuid not null references agents (id)
);
