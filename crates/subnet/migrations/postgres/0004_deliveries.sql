-- What the switchboard delivered, and how each delivery went.
create table deliveries (
    id bigserial primary key,
    route text not null,
    at timestamptz not null default now(),
    payload jsonb not null,
    outcomes jsonb not null
);

create index deliveries_route on deliveries (route, id desc);
