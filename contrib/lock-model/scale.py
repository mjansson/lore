#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""contrib/lock-model/scale.py — the successor-locks lock table at scale.

verify.sh checks what the model means and what one operation costs. Two
questions it leaves open are about aggregates rather than single calls, and
both need loops, which is why they live here:

  Query   what listing a scope costs once the scope holds a large number of
          live locks — the call an editor makes to populate its UI, and the
          only unmeasured operation on a routine path.

  Claim   whether two transactions that want overlapping files converge or
          livelock. The design's answer is ascending key order plus a
          transaction-id tiebreak; this races real claimers to find out, and
          races them again in random order as the control. The control is
          expected to livelock — that is the result, and the reason the order
          is part of the protocol rather than a client convention.

Usage:
  docker run -d --name lore-ddb -p 8000:8000 amazon/dynamodb-local:latest \
    -jar DynamoDBLocal.jar -inMemory -sharedDb
  python3 contrib/lock-model/scale.py
  docker rm -f lore-ddb

Exit codes:
  0  every check passed
  1  a check failed; the failing expectation is printed
  2  DynamoDB Local is not reachable at $DDB_ENDPOINT
"""

import base64
import json
import os
import random
import struct
import sys
import threading
import time
import urllib.error
import urllib.request

ENDPOINT = os.environ.get("DDB_ENDPOINT", "http://localhost:8000")
TABLE = "scale-locks"
SCOPE = "scope-main#07"

failures = 0


def check(description, expected, actual):
    global failures
    if expected == actual:
        print(f"  ok    {description:<46} {actual}")
    else:
        print(f"  FAIL  {description:<46} expected {expected}, got {actual}")
        failures += 1


def note(description, value):
    print(f"  --    {description:<46} {value}")


def ddb(target, body):
    """One DynamoDB call. DynamoDB Local accepts any signature, so the
    Authorization header is a placeholder rather than SigV4."""
    request = urllib.request.Request(
        ENDPOINT + "/",
        data=json.dumps(body).encode(),
        headers={
            "Content-Type": "application/x-amz-json-1.0",
            "X-Amz-Target": f"DynamoDB_20120810.{target}",
            "Authorization": (
                "AWS4-HMAC-SHA256 Credential=local/20260101/us-east-2/dynamodb/"
                "aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, "
                "Signature=placeholder"
            ),
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        return json.load(error)


def key(index):
    """A 16-byte file id, as the model keys on."""
    return base64.b64encode(struct.pack(">QQ", 0, index)).decode()


def create_table():
    ddb("DeleteTable", {"TableName": TABLE})
    ddb(
        "CreateTable",
        {
            "TableName": TABLE,
            "BillingMode": "PAY_PER_REQUEST",
            "AttributeDefinitions": [
                {"AttributeName": "k", "AttributeType": "B"},
                {"AttributeName": "held_pk", "AttributeType": "S"},
                {"AttributeName": "held_sk", "AttributeType": "S"},
                {"AttributeName": "scope_pk", "AttributeType": "S"},
                {"AttributeName": "scope_sk", "AttributeType": "S"},
            ],
            "KeySchema": [{"AttributeName": "k", "KeyType": "HASH"}],
            "GlobalSecondaryIndexes": [
                {
                    "IndexName": "held",
                    "KeySchema": [
                        {"AttributeName": "held_pk", "KeyType": "HASH"},
                        {"AttributeName": "held_sk", "KeyType": "RANGE"},
                    ],
                    "Projection": {"ProjectionType": "KEYS_ONLY"},
                },
                {
                    "IndexName": "scope",
                    "KeySchema": [
                        {"AttributeName": "scope_pk", "KeyType": "HASH"},
                        {"AttributeName": "scope_sk", "KeyType": "RANGE"},
                    ],
                    "Projection": {"ProjectionType": "KEYS_ONLY"},
                },
            ],
        },
    )


def require_dynamodb():
    if "TableNames" not in ddb("ListTables", {}):
        print(
            f"DynamoDB Local unreachable at {ENDPOINT} — see the usage note above.",
            file=sys.stderr,
        )
        sys.exit(2)


def seed_locks(count, threads=16):
    """`count` held locks in one scope, written 25 at a time.

    Every row carries both index key pairs, which is what a live lock on a
    file that also has chain state looks like."""
    per_batch = 25
    batches = [
        list(range(start, min(start + per_batch, count)))
        for start in range(0, count, per_batch)
    ]
    lock = threading.Lock()
    remaining = list(batches)

    def worker():
        while True:
            with lock:
                if not remaining:
                    return
                batch = remaining.pop()
            ddb(
                "BatchWriteItem",
                {
                    "RequestItems": {
                        TABLE: [
                            {
                                "PutRequest": {
                                    "Item": {
                                        "k": {"B": key(index)},
                                        "holder": {"B": key(1)},
                                        "owner_id": {"S": "seed"},
                                        "confirmed": {"BOOL": True},
                                        "expires_at": {"N": "9999999999"},
                                        "revision": {"B": key(2)},
                                        "content": {"B": key(3)},
                                        "held_pk": {"S": "repo-x#07"},
                                        "held_sk": {"S": f"branch-main#{index:08d}"},
                                        "scope_pk": {"S": SCOPE},
                                        "scope_sk": {"S": f"file-{index:08d}"},
                                    }
                                }
                            }
                            for index in batch
                        ]
                    }
                },
            )

    pool = [threading.Thread(target=worker) for _ in range(threads)]
    for thread in pool:
        thread.start()
    for thread in pool:
        thread.join()


def query_scope(page_limit=None):
    """Page the scope index as a server-streaming Query would, and total what
    it reads."""
    pages, items, capacity = 0, 0, 0.0
    start_key = None
    while True:
        body = {
            "TableName": TABLE,
            "IndexName": "scope",
            "KeyConditionExpression": "scope_pk = :s",
            "ExpressionAttributeValues": {":s": {"S": SCOPE}},
            "ReturnConsumedCapacity": "INDEXES",
        }
        if page_limit:
            body["Limit"] = page_limit
        if start_key:
            body["ExclusiveStartKey"] = start_key
        response = ddb("Query", body)
        pages += 1
        items += response.get("Count", 0)
        consumed = response.get("ConsumedCapacity")
        if consumed:
            consumed = consumed[0] if isinstance(consumed, list) else consumed
            capacity += consumed.get("CapacityUnits", 0.0)
        start_key = response.get("LastEvaluatedKey")
        if not start_key:
            return pages, items, capacity


def claim(file_key, transaction, now, expires):
    """One claim: the conditional update the design acquires with. Returns the
    holding transaction id when the condition fails."""
    response = ddb(
        "UpdateItem",
        {
            "TableName": TABLE,
            "Key": {"k": {"B": file_key}},
            "ConditionExpression": "attribute_not_exists(holder) OR expires_at < :now",
            "UpdateExpression": (
                "SET holder=:h, owner_id=:o, confirmed=:c, expires_at=:e, "
                "held_pk=:hp, held_sk=:hs"
            ),
            "ExpressionAttributeValues": {
                ":now": {"N": str(now)},
                ":h": {"B": file_key},
                ":o": {"S": transaction},
                ":c": {"BOOL": False},
                ":e": {"N": str(expires)},
                ":hp": {"S": "repo-x#07"},
                ":hs": {"S": f"branch-{transaction}"},
            },
            "ReturnValuesOnConditionCheckFailure": "ALL_OLD",
        },
    )
    if "__type" in response:
        holder = response.get("Item", {}).get("owner_id", {}).get("S")
        return False, holder
    return True, None


def abort(keys, transaction):
    """Drop exactly this transaction's claims, as `abort` does."""
    for file_key in keys:
        ddb(
            "DeleteItem",
            {
                "TableName": TABLE,
                "Key": {"k": {"B": file_key}},
                "ConditionExpression": "owner_id = :o",
                "ExpressionAttributeValues": {":o": {"S": transaction}},
            },
        )


def race(workers, keys, ascending, attempt_cap=200):
    """Race `workers` transactions for the same `keys`.

    Every worker wants every key. On contention the design's rule decides:
    with a total order on transaction ids, the lower id holds its ground and
    the higher id drops what it has and starts over, so the lowest contender
    always makes progress. `ascending` selects the key order under test —
    false is the control, and the reason the order is part of the protocol."""
    ddb("DeleteTable", {"TableName": TABLE})
    create_table()

    results = {}
    lock = threading.Lock()

    def worker(index):
        transaction = f"t{index:04d}"
        order = list(keys)
        if not ascending:
            random.Random(index).shuffle(order)
        attempts, held = 0, []
        while attempts < attempt_cap:
            attempts += 1
            now = int(time.time())
            granted, conflict = True, None
            held = []
            for file_key in order:
                ok, holder = claim(file_key, transaction, now, now + 300)
                if ok:
                    held.append(file_key)
                    continue
                granted, conflict = False, holder
                break
            if granted:
                with lock:
                    results[transaction] = (attempts, len(held))
                abort(held, transaction)  # conclude: release what it took
                return
            # The tiebreak: a lower id keeps what it has and waits; a higher
            # id yields, which is what makes the cycle terminate.
            if conflict is not None and transaction < conflict:
                time.sleep(0.01)
            else:
                abort(held, transaction)
                time.sleep(0.01 * random.random())
        with lock:
            results[transaction] = (attempts, -1)

    pool = [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
    started = time.time()
    for thread in pool:
        thread.start()
    for thread in pool:
        thread.join()
    elapsed = time.time() - started

    completed = sum(1 for attempts, held in results.values() if held >= 0)
    worst = max(attempts for attempts, _ in results.values())
    return completed, worst, elapsed


def main():
    locks = int(os.environ.get("SCALE_LOCKS", "100000"))
    workers = int(os.environ.get("SCALE_WORKERS", "8"))
    contended = int(os.environ.get("SCALE_CONTENDED_KEYS", "40"))

    print(f"successor-locks lock table at scale against {ENDPOINT}\n")
    require_dynamodb()

    print(f"Query — listing a scope holding {locks:,} live locks")
    create_table()
    seeded = time.time()
    seed_locks(locks)
    note("seeding took", f"{time.time() - seeded:.1f}s")

    pages, items, capacity = query_scope()
    check("every lock is listed", locks, items)
    note("pages the scope index returned", pages)
    note("read units for the whole scope", f"{capacity:.1f}")
    note("read units per 1,000 locks", f"{capacity / max(locks, 1) * 1000:.2f}")

    # A KEYS_ONLY index row is small, so a page carries a lot of them. That is
    # the property that makes this a routine call rather than a scan.
    note("locks per page", f"{items // max(pages, 1):,}")

    pages_100, _, _ = query_scope(page_limit=100)
    note("pages at a 100-item client limit", pages_100)

    print(f"\nclaim convergence — {workers} transactions wanting the same {contended} files")
    completed, worst, elapsed = race(workers, [key(i) for i in range(contended)], True)
    check("ascending order: every transaction completed", workers, completed)
    note("worst-case attempts by one transaction", worst)
    note("wall time", f"{elapsed:.1f}s")

    # The control. Nothing about the claim itself changes — only the order the
    # keys are taken in — so whatever separates the two runs is the ordering
    # rule doing its work.
    completed_r, worst_r, elapsed_r = race(workers, [key(i) for i in range(contended)], False)
    note("random order: transactions that completed", f"{completed_r} of {workers}")
    note("worst-case attempts by one transaction", worst_r)
    note("wall time", f"{elapsed_r:.1f}s")
    check(
        "ordering is what converges, not the tiebreak",
        True,
        completed > completed_r or worst < worst_r,
    )

    print()
    if failures:
        print(f"{failures} check(s) failed")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
