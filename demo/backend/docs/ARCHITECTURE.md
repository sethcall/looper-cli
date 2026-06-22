# Backend — Architecture

A small set of services over Postgres. Stateless handlers, a shared domain crate, migrations in CI.
Background work runs on a queue; nothing blocks the request path.
