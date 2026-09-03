#!/usr/bin/env bash
#
# contrib/lock-model/verify.sh — verify the successor-locks DynamoDB model.
#
# The successor-locks design keeps a file's lock and its chain state in one
# item, so acquisition is a single conditional update that takes the lock and
# returns the state the causality check compares against. That claim is about
# what the DynamoDB API does, so this checks it against DynamoDB rather than
# reasoning about it, and reports the write capacity each operation consumes.
#
# It builds two tables: the model the design proposes, and today's deployed
# `locks` table from contrib/aws/storage.tf, so the costs can be compared
# directly.
#
# Usage:
#   docker run -d --name lore-ddb -p 8000:8000 amazon/dynamodb-local:latest \
#     -jar DynamoDBLocal.jar -inMemory -sharedDb
#   bash contrib/lock-model/verify.sh
#   docker rm -f lore-ddb
#
# Exit codes:
#   0  every check passed
#   1  a check failed; the failing expectation is printed
#   2  DynamoDB Local is not reachable at $DDB_ENDPOINT

set -uo pipefail

readonly DDB_ENDPOINT="${DDB_ENDPOINT:-http://localhost:8000}"
readonly SPEC_TABLE="spec-locks"
readonly TODAY_TABLE="today-locks"

failures=0

# ddb TARGET JSON — one DynamoDB API call. DynamoDB Local accepts any
# signature, so the Authorization header is a placeholder rather than SigV4.
ddb() {
    curl -s -X POST "${DDB_ENDPOINT}/" \
        -H "Content-Type: application/x-amz-json-1.0" \
        -H "X-Amz-Target: DynamoDB_20120810.$1" \
        -H "Authorization: AWS4-HMAC-SHA256 Credential=local/20260101/us-east-2/dynamodb/aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, Signature=placeholder" \
        -d "$2"
}

# check DESCRIPTION EXPECTED ACTUAL
check() {
    if [[ "$2" == "$3" ]]; then
        printf '  ok    %-46s %s\n' "$1" "$3"
    else
        printf '  FAIL  %-46s expected %s, got %s\n' "$1" "$2" "$3"
        failures=$((failures + 1))
    fi
}

# capacity JSON — total write units a response reports.
capacity() {
    python3 -c 'import sys,json; d=json.load(sys.stdin); c=d.get("ConsumedCapacity"); c=c[0] if isinstance(c,list) else c; print(c["CapacityUnits"] if c else "none")'
}

# Each reader takes a response on stdin and prints one value, or "absent" when
# the response doesn't carry it. Keeping them separate avoids passing Python
# expressions through nested command substitution, which quotes badly.

# count — the Count of a Scan or Query.
count() {
    python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("Count","absent"))'
}

# error — the exception name a response carries, or "none" for success.
error() {
    python3 -c 'import sys,json;print(json.load(sys.stdin).get("__type","none").split("#")[-1])'
}

# binary SECTION NAME — a binary attribute, decoded to text. SECTION is
# Attributes (a returned prior state) or Item (a failed condition's row).
binary() {
    python3 -c '
import sys, json, base64
section, name = sys.argv[1], sys.argv[2]
d = json.load(sys.stdin)
try:
    print(base64.b64decode(d[section][name]["B"]).decode())
except (KeyError, TypeError):
    print("absent")' "$1" "$2"
}

# has_attributes — whether a response returned a prior state at all.
has_attributes() {
    python3 -c 'import sys,json;print("present" if json.load(sys.stdin).get("Attributes") else "absent")'
}

require_dynamodb() {
    if ! ddb ListTables '{}' | grep -q TableNames; then
        echo "DynamoDB Local unreachable at ${DDB_ENDPOINT} — see the usage note above." >&2
        exit 2
    fi
}

create_spec_table() {
    ddb DeleteTable "{\"TableName\":\"${SPEC_TABLE}\"}" >/dev/null 2>&1
    ddb CreateTable "{
      \"TableName\":\"${SPEC_TABLE}\", \"BillingMode\":\"PAY_PER_REQUEST\",
      \"AttributeDefinitions\":[
        {\"AttributeName\":\"k\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"held_pk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"held_sk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"scope_pk\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"scope_sk\",\"AttributeType\":\"S\"}],
      \"KeySchema\":[{\"AttributeName\":\"k\",\"KeyType\":\"HASH\"}],
      \"GlobalSecondaryIndexes\":[
        {\"IndexName\":\"held\",
         \"KeySchema\":[{\"AttributeName\":\"held_pk\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"held_sk\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"KEYS_ONLY\"}},
        {\"IndexName\":\"scope\",
         \"KeySchema\":[{\"AttributeName\":\"scope_pk\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"scope_sk\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"KEYS_ONLY\"}}]}" >/dev/null
}

# Today's table, matching contrib/aws/storage.tf: three ALL-projected indexes
# over a path-hash primary key.
create_today_table() {
    ddb DeleteTable "{\"TableName\":\"${TODAY_TABLE}\"}" >/dev/null 2>&1
    ddb CreateTable "{
      \"TableName\":\"${TODAY_TABLE}\", \"BillingMode\":\"PAY_PER_REQUEST\",
      \"AttributeDefinitions\":[
        {\"AttributeName\":\"hash\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"repositoryBranch\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"ownerId\",\"AttributeType\":\"S\"},
        {\"AttributeName\":\"repository\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"branch\",\"AttributeType\":\"B\"},
        {\"AttributeName\":\"description\",\"AttributeType\":\"S\"}],
      \"KeySchema\":[{\"AttributeName\":\"hash\",\"KeyType\":\"HASH\"},
                     {\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"RANGE\"}],
      \"GlobalSecondaryIndexes\":[
        {\"IndexName\":\"owner-repo-branch\",
         \"KeySchema\":[{\"AttributeName\":\"ownerId\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}},
        {\"IndexName\":\"repo-branch\",
         \"KeySchema\":[{\"AttributeName\":\"repository\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"branch\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}},
        {\"IndexName\":\"repo-branch-description\",
         \"KeySchema\":[{\"AttributeName\":\"repositoryBranch\",\"KeyType\":\"HASH\"},
                        {\"AttributeName\":\"description\",\"KeyType\":\"RANGE\"}],
         \"Projection\":{\"ProjectionType\":\"ALL\"}}]}" >/dev/null
}

# A chain entry with no lock: what a concluded session leaves behind.
seed_chain_entry() {
    ddb PutItem "{\"TableName\":\"${SPEC_TABLE}\",\"Item\":{
      \"k\":{\"B\":\"AAAAAAAAAAE=\"},
      \"revision\":{\"B\":\"cmV2MDE=\"},
      \"content\":{\"B\":\"Y29udDAx\"},
      \"scope_pk\":{\"S\":\"scope-main#07\"},
      \"scope_sk\":{\"S\":\"file-0001\"}}}" >/dev/null
}

acquire() {
    ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"$1\"}},
      \"ConditionExpression\":\"attribute_not_exists(holder) OR expires_at < :now\",
      \"UpdateExpression\":\"SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, held_pk=:hp, held_sk=:hs\",
      \"ExpressionAttributeValues\":{
        \":now\":{\"N\":\"$2\"}, \":h\":{\"B\":\"$3\"}, \":o\":{\"S\":\"tester\"},
        \":c\":{\"BOOL\":false}, \":e\":{\"N\":\"$4\"},
        \":hp\":{\"S\":\"repo-x#07\"}, \":hs\":{\"S\":\"$5\"}},
      \"ReturnValues\":\"ALL_OLD\",
      \"ReturnValuesOnConditionCheckFailure\":\"ALL_OLD\",
      \"ReturnConsumedCapacity\":\"INDEXES\"}"
}


# Every call below captures its response to a variable before reading it.
# Nesting a ddb call inside check's own command substitution quotes badly, and
# the failure looks like a model problem rather than a shell one.

index_count() {
    ddb Scan "{\"TableName\":\"${SPEC_TABLE}\",\"IndexName\":\"$1\",\"Select\":\"COUNT\"}"
}

query_held() {
    ddb Query "{\"TableName\":\"${SPEC_TABLE}\",\"IndexName\":\"held\",
      \"KeyConditionExpression\":\"$1\",\"ExpressionAttributeValues\":$2}"
}

delete_guarded() {
    ddb DeleteItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"$1\"}},
      \"ConditionExpression\":\"holder = :h AND attribute_not_exists(revision)\",
      \"ExpressionAttributeValues\":{\":h\":{\"B\":\"$2\"}}}"
}

echo "successor-locks DynamoDB model verification against ${DDB_ENDPOINT}"
echo
require_dynamodb
create_spec_table
create_today_table
seed_chain_entry

echo "sparse indexes — a chain entry carries no lock, so it indexes only as chain"
r="$(index_count held)";  check "chain-only row in held index"  "0" "$(printf '%s' "$r" | count)"
r="$(index_count scope)"; check "chain-only row in scope index" "1" "$(printf '%s' "$r" | count)"

echo
echo "acquire — one conditional update takes the lock and returns the chain"
r="$(acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUE=" 9999 "branch-A#file-0001")"
check "ALL_OLD carries the chain revision" "rev01"  "$(printf '%s' "$r" | binary Attributes revision)"
check "ALL_OLD carries the chain content"  "cont01" "$(printf '%s' "$r" | binary Attributes content)"
check "write units to acquire"             "2.0"    "$(printf '%s' "$r" | capacity)"
r="$(index_count held)"; check "held index now holds the lock" "1" "$(printf '%s' "$r" | count)"

echo
echo "contention — the denial has to name the holder"
r="$(acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUI=" 9999 "branch-B#file-0001")"
check "refused"                          "ConditionalCheckFailedException" "$(printf '%s' "$r" | error)"
check "denial carries the current holder" "branch-A"                       "$(printf '%s' "$r" | binary Item holder)"

echo
echo "expiry is enforced on read, so no TTL is required"
r="$(acquire "AAAAAAAAAAE=" 99999 "YnJhbmNoLUI=" 199999 "branch-B#file-0001")"
check "stale holder is displaced" "rev01" "$(printf '%s' "$r" | binary Attributes revision)"

echo
echo "first edit — a file with no row at all"
r="$(acquire "AAAAAAAAAAI=" 1000 "YnJhbmNoLUM=" 9999 "branch-C#file-0002")"
check "no prior state to compare" "absent" "$(printf '%s' "$r" | has_attributes)"

echo
echo "confirm — KEYS_ONLY means flipping a boolean touches no index"
r="$(ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"AAAAAAAAAAE=\"}},
  \"UpdateExpression\":\"SET confirmed=:c\",\"ExpressionAttributeValues\":{\":c\":{\"BOOL\":true}},
  \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "write units to confirm" "1.0" "$(printf '%s' "$r" | capacity)"

echo
echo "conclude — advance the chain and drop the lock in one write"
r="$(ddb UpdateItem "{\"TableName\":\"${SPEC_TABLE}\",\"Key\":{\"k\":{\"B\":\"AAAAAAAAAAE=\"}},
  \"ConditionExpression\":\"holder = :h\",
  \"UpdateExpression\":\"SET revision=:r, content=:c, scope_pk=:sp, scope_sk=:ss REMOVE holder, owner_id, confirmed, expires_at, held_pk, held_sk\",
  \"ExpressionAttributeValues\":{\":h\":{\"B\":\"YnJhbmNoLUI=\"},\":r\":{\"B\":\"cmV2MDI=\"},
    \":c\":{\"B\":\"Y29udDAy\"},\":sp\":{\"S\":\"scope-main#07\"},\":ss\":{\"S\":\"file-0001\"}},
  \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "write units to conclude" "2.0" "$(printf '%s' "$r" | capacity)"
r="$(index_count held)"; check "lock left the held index" "1" "$(printf '%s' "$r" | count)"

echo
echo "release on a mergeable file — the guard cannot reach chain data"
# file-0002 holds a lock and no chain: what a legacy lock on a mergeable file leaves.
r="$(delete_guarded "AAAAAAAAAAI=" "YnJhbmNoLUM=")"
check "lock-only row is deleted" "none" "$(printf '%s' "$r" | error)"
# Aim the same guard at a file that does have a chain: the guard alone must stop it.
acquire "AAAAAAAAAAE=" 1000 "YnJhbmNoLUQ=" 9999 "branch-D#file-0001" >/dev/null
r="$(delete_guarded "AAAAAAAAAAE=" "YnJhbmNoLUQ=")"
check "delete refused where a chain exists" "ConditionalCheckFailedException" "$(printf '%s' "$r" | error)"

echo
echo "one index serves both lock queries"
r="$(query_held "held_pk = :p" '{":p":{"S":"repo-x#07"}}')"
check "Query(repository)" "1" "$(printf '%s' "$r" | count)"
r="$(query_held "held_pk = :p AND begins_with(held_sk, :b)" '{":p":{"S":"repo-x#07"},":b":{"S":"branch-D#"}}')"
check "Query(branch) by sort-key prefix" "1" "$(printf '%s' "$r" | count)"
r="$(query_held "held_pk = :p AND begins_with(held_sk, :b)" '{":p":{"S":"repo-x#07"},":b":{"S":"branch-Z#"}}')"
check "Query(branch) misses another branch" "0" "$(printf '%s' "$r" | count)"

echo
echo "BatchWriteItem carries no conditions, and never says so"
python3 -c "
import json, base64
items = [{'PutRequest': {'Item': {'k': {'B': base64.b64encode(bytes([0,0,0,0,0,0,9,i])).decode()}}}} for i in range(26)]
open('/tmp/lock-model-batch26.json','w').write(json.dumps({'RequestItems': {'${SPEC_TABLE}': items}}))"
r="$(ddb BatchWriteItem "$(cat /tmp/lock-model-batch26.json)")"
check "26 items is rejected" "ValidationException" "$(printf '%s' "$r" | error)"
rm -f /tmp/lock-model-batch26.json
# The trap: a condition on a batched write is accepted and silently inert, so a
# batched claim path would review as correct and grant one lock twice.
r="$(ddb BatchWriteItem "{\"RequestItems\":{\"${SPEC_TABLE}\":[{\"PutRequest\":{\"Item\":{
  \"k\":{\"B\":\"AAAAAAAACRA=\"},
  \"ConditionExpression\":{\"S\":\"attribute_not_exists(holder)\"}}}}]}}")"
check "a condition on a batched put is not refused" "none" "$(printf '%s' "$r" | error)"

echo
echo "cost against the table this replaces"
r="$(ddb TransactWriteItems "{\"TransactItems\":[{\"Put\":{
  \"TableName\":\"${TODAY_TABLE}\",
  \"Item\":{\"hash\":{\"B\":\"aGFzaG9mcGF0aDAwMDAwMDAwMDAwMDA=\"},
            \"repositoryBranch\":{\"B\":\"cmVwb2JyYW5jaDAwMDAwMDAwMDAw\"},
            \"repository\":{\"B\":\"cmVwbzAwMDAwMDAwMDAwMDAwMDA=\"},
            \"branch\":{\"B\":\"YnJhbmNoMDAwMDAwMDAwMDAwMDA=\"},
            \"ownerId\":{\"S\":\"tester\"},
            \"description\":{\"S\":\"Content/Characters/Hero/Meshes/SK_Hero.uasset\"},
            \"timestamp\":{\"S\":\"2026-01-01T00:00:00Z\"}},
  \"ConditionExpression\":\"attribute_not_exists(#pk)\",
  \"ExpressionAttributeNames\":{\"#pk\":\"hash\"}}}],
 \"ReturnConsumedCapacity\":\"INDEXES\"}")"
check "today: write units to acquire" "8.0" "$(printf '%s' "$r" | capacity)"

echo
if (( failures == 0 )); then
    echo "all checks passed"
    exit 0
fi
echo "${failures} check(s) failed"
exit 1
