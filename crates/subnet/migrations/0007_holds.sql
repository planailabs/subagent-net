-- Holds: while frozen, the deliveries of routes going through one wait in
-- it (`held`, in `seq` order) until it's released.
create table holds (
    name text primary key,
    frozen boolean not null,
    since timestamptz
);

create table held (
    hold text not null,
    seq bigint not null,
    route text not null,
    payload jsonb not null,
    primary key (hold, seq)
);
