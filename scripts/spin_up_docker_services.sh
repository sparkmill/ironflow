#!/usr/bin/env bash
set -euo pipefail

docker compose up -d

# Wait for PostgreSQL to be ready
echo "Waiting for PostgreSQL to be ready..."
max_attempts=60
attempt=0

# Wait for PostgreSQL to be ready using docker compose exec
while ! docker compose exec -T postgres pg_isready -U postgres -d postgres > /dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ $attempt -eq $max_attempts ]; then
        echo "Timed out waiting for PostgreSQL to be ready after ${max_attempts} seconds"
        echo "Container logs:"
        docker compose logs postgres
        echo "Container status:"
        docker compose ps
        exit 1
    fi
    echo "Waiting for PostgreSQL... (attempt $attempt/$max_attempts)"
    sleep 1
done

echo "PostgreSQL is ready!"

# Additional health check to ensure database is fully operational
docker compose exec -T postgres psql -U postgres -c "SELECT 1" > /dev/null 2>&1
if [ $? -eq 0 ]; then
    echo "PostgreSQL is accepting connections!"
else
    echo "PostgreSQL is not accepting connections"
    docker compose logs postgres
    exit 1
fi

# Give it a moment to ensure all initialization scripts have run
sleep 2
