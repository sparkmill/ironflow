#!/bin/bash
# Format or check SQL files using pgFormatter
# Usage: ./format-sql.sh [--check] [--debug]

set -e

# Parse arguments
CHECK_MODE=false
DEBUG_MODE=false
for arg in "$@"; do
    case $arg in
    --check)
        CHECK_MODE=true
        ;;
    --debug)
        DEBUG_MODE=true
        export DEBUG=1
        ;;
    esac
done

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Check if pg_format is installed
if ! command -v pg_format &>/dev/null; then
    echo -e "${RED}❌ pgFormatter is not installed${NC}"
    echo "Install it with one of:"
    echo "  brew install pgformatter     # macOS"
    echo "  apt-get install pgformatter  # Ubuntu/Debian"
    echo "  npm install -g pgformatter   # Cross-platform"
    exit 1
fi

# Find all SQL files
SQL_FILES=$(find crates/ironflow/migrations -name "*.sql" 2>/dev/null || true)

if [ -z "$SQL_FILES" ]; then
    echo -e "${YELLOW}⚠️  No SQL files found${NC}"
    exit 0
fi

if [ "$CHECK_MODE" = true ]; then
    # Check mode: verify formatting without modifying files
    echo -e "${GREEN}🔍 Checking SQL formatting...${NC}"
    FAILED=false

    for file in $SQL_FILES; do
        echo "  Checking: $file"

        # Format the file, capturing stdout only
        FORMATTED=$(pg_format "$file" 2>/tmp/pg_format_err.$$)
        PG_EXIT_CODE=$?

        # Check if pg_format failed (e.g., syntax error)
        if [ $PG_EXIT_CODE -ne 0 ]; then
            echo -e "${RED}    ❌ pg_format error:${NC}"
            # Show the actual error message from stderr
            cat /tmp/pg_format_err.$$
            rm -f /tmp/pg_format_err.$$
            FAILED=true
            continue
        fi
        rm -f /tmp/pg_format_err.$$

        # Compare formatted output with original file
        if ! echo "$FORMATTED" | diff -q "$file" - >/dev/null 2>&1; then
            echo -e "${RED}    ❌ Formatting issues found${NC}"
            FAILED=true

            # Show the diff always in check mode to help debug
            echo "    Diff:"
            echo "$FORMATTED" | diff -u "$file" - || true

            # Also show a character-level comparison for debugging
            if [ -n "$DEBUG" ]; then
                echo "    Debug: First 200 chars of original:"
                head -c 200 "$file" | od -c
                echo "    Debug: First 200 chars of formatted:"
                echo "$FORMATTED" | head -c 200 | od -c
            fi
        else
            echo -e "${GREEN}    ✅ Properly formatted${NC}"
        fi
    done

    if [ "$FAILED" = true ]; then
        echo -e "\n${RED}❌ SQL formatting check failed!${NC}"
        echo "Run './scripts/format-sql.sh' locally to fix formatting"
        exit 1
    else
        echo -e "\n${GREEN}✅ All SQL files are properly formatted!${NC}"
    fi
else
    # Format mode: modify files in place
    echo -e "${GREEN}🔧 Formatting SQL files...${NC}"
    for file in $SQL_FILES; do
        echo "  Formatting: $file"
        # Redirect stderr to /dev/null to suppress pg_format usage output
        pg_format -i "$file" 2>/dev/null
    done
    echo -e "${GREEN}✅ SQL formatting complete!${NC}"
fi
