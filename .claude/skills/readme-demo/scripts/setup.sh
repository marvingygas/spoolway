#!/usr/bin/env bash
# Builds the "shop" demo project the README GIF records: a small Node app at
# ~/demo/shop, the impl / impl_fast / impl_tdd pipelines, the cart and checkout
# groups in pending (not queued), three routines and two jobs.
#
# Usage: setup.sh            Refuses to run if ~/demo already exists.
# Prints the project's home directory (~/.spoolway/shop-<id>) on the last line.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

DEMO="$HOME/demo"
REPO="$DEMO/shop"
SPOOLWAY="${SPOOLWAY:-spoolway}"
# Every step runs Claude Haiku 5.5: cheap and quick enough to record live, and
# unlike Haiku 4.5 it supports Claude Code's auto mode, so lanes never stop
# on an approval prompt.
MODEL="claude-haiku-5-5"

if [[ -e "$DEMO" ]]; then
  echo "$DEMO already exists. Run teardown.sh first." >&2
  exit 1
fi

mkdir -p "$REPO"
cd "$REPO"
git init -q -b main
git config user.name "$(git config --global user.name || echo demo)"
git config user.email "$(git config --global user.email || echo demo@example.invalid)"
echo "# shop" > README.md
git add -A && git commit -qm init

"$SPOOLWAY" init --yes --provider claude --tracker none >/dev/null
ID=$(cat .git/spoolway-id)
HOME_DIR="$HOME/.spoolway/shop-$ID"

# --- the app ----------------------------------------------------------------
mkdir -p src test docs
cat > package.json <<'EOF'
{
  "name": "shop",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {
    "test": "node --test"
  }
}
EOF
cat > README.md <<'EOF'
# shop

A tiny storefront: a cart, a checkout and a login. No dependencies.

```
npm test
```
EOF
cat > src/cart.js <<'EOF'
// A cart is a list of lines: { sku, name, price, qty }. Prices are in cents.

export function createCart() {
  return { lines: [] };
}

export function addItem(cart, { sku, name, price }, qty = 1) {
  const line = cart.lines.find((l) => l.sku === sku);
  if (line) {
    line.qty += qty;
  } else {
    cart.lines.push({ sku, name, price, qty });
  }
  return cart;
}

export function removeItem(cart, sku) {
  cart.lines = cart.lines.filter((l) => l.sku !== sku);
  return cart;
}

export function subtotal(cart) {
  return cart.lines.reduce((sum, l) => sum + l.price * l.qty, 0);
}
EOF
cat > src/render.js <<'EOF'
import { subtotal } from "./cart.js";

const money = (cents) => `$${(cents / 100).toFixed(2)}`;

// Renders the cart page body as HTML.
export function renderCart(cart) {
  if (cart.lines.length === 0) {
    return "";
  }
  const rows = cart.lines
    .map((l) => `<li>${l.name} × ${l.qty} — ${money(l.price * l.qty)}</li>`)
    .join("\n");
  return `<ul>\n${rows}\n</ul>\n<p>Subtotal: ${money(subtotal(cart))}</p>`;
}
EOF
cat > src/auth.js <<'EOF'
import { randomUUID } from "node:crypto";

const users = new Map([["ada@example.com", "lovelace"]]);
const sessions = new Map();

export function login(email, password) {
  if (users.get(email) !== password) {
    throw new Error("invalid credentials");
  }
  const token = randomUUID();
  sessions.set(token, { email, createdAt: Date.now() });
  return token;
}

export function logout(token) {
  sessions.delete(token);
}

export function sessionFor(token) {
  return sessions.get(token) ?? null;
}
EOF
cat > src/checkout.js <<'EOF'
import { subtotal } from "./cart.js";

// Places an order for the cart. Payment is not wired up yet: the order is
// recorded as unpaid.
export function checkout(cart, { token } = {}) {
  if (cart.lines.length === 0) {
    throw new Error("cart is empty");
  }
  return {
    id: `ord_${Date.now()}`,
    token,
    lines: cart.lines.map((l) => ({ ...l })),
    amount: subtotal(cart),
    status: "unpaid",
  };
}
EOF
cat > test/cart.test.js <<'EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { createCart, addItem, removeItem, subtotal } from "../src/cart.js";

const mug = { sku: "mug", name: "Mug", price: 1200 };

test("adding the same sku twice bumps the quantity", () => {
  const cart = addItem(addItem(createCart(), mug), mug);
  assert.equal(cart.lines.length, 1);
  assert.equal(cart.lines[0].qty, 2);
});

test("subtotal sums price times quantity", () => {
  const cart = addItem(createCart(), mug, 3);
  assert.equal(subtotal(cart), 3600);
});

test("removing a sku drops its line", () => {
  const cart = removeItem(addItem(createCart(), mug), "mug");
  assert.equal(cart.lines.length, 0);
});
EOF
cat > test/auth.test.js <<'EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { login, logout, sessionFor } from "../src/auth.js";

test("a good password opens a session", () => {
  const token = login("ada@example.com", "lovelace");
  assert.equal(sessionFor(token).email, "ada@example.com");
});

test("a bad password is refused", () => {
  assert.throws(() => login("ada@example.com", "nope"));
});

test("logout closes the session", () => {
  const token = login("ada@example.com", "lovelace");
  logout(token);
  assert.equal(sessionFor(token), null);
});
EOF
cat > test/checkout.test.js <<'EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { createCart, addItem } from "../src/cart.js";
import { checkout } from "../src/checkout.js";

test("checkout records an unpaid order for the subtotal", () => {
  const cart = addItem(createCart(), { sku: "mug", name: "Mug", price: 1200 }, 2);
  const order = checkout(cart);
  assert.equal(order.amount, 2400);
  assert.equal(order.status, "unpaid");
});

test("an empty cart cannot check out", () => {
  assert.throws(() => checkout(createCart()));
});
EOF
echo "node_modules/" > .gitignore
npm test >/dev/null 2>&1 || { echo "the demo app's own tests fail" >&2; exit 1; }

# --- pipelines --------------------------------------------------------------
# Keep init's reference header on each file, then swap in the demo's own
# pipelines. Each ends with `npm test` instead of a pull request: no remote.
cd .spoolway/pipelines
HDR=$(sed -n '/^# Full reference/,/^# <<< spoolway <<</p' default.yml)
rm -f default.yml bugfix.yml
{ echo "$HDR"; cat <<EOF

version: "1.1"

description: >-
  One unit of feature work, start to finish: implement against the acceptance
  criteria, review the diff, document what changed, and let the test suite
  give the final verdict.

task_template: default

steps:
  - id: implement
    description: Write the code to satisfy the task's acceptance criteria.
    agent: claude
    prompt: implementer
    model: $MODEL
    session: true
    loop: 2
    on_pass: review

  - id: review
    description: Check the diff against the acceptance criteria and project standards.
    agent: claude
    prompt: reviewer
    model: $MODEL
    session: true
    on_pass: document
    on_fail: implement

  - id: document
    description: Bring the domain documents in line with what this task changed.
    agent: claude
    prompt: archivist
    model: $MODEL
    on_pass: test

  - id: test
    description: The mechanical verdict on this change, as an exit code.
    run: npm test
    timeout: 5m
    on_pass: done
    on_fail: implement
EOF
} > impl.yml
{ echo "$HDR"; cat <<EOF

version: "1.0"

description: >-
  One small change with a single agent step and no review. For copy,
  empty states and other changes a test run is enough to judge.

task_template: default

steps:
  - id: implement
    description: >-
      Write the whole change to satisfy the task's acceptance criteria, and
      leave it review-ready — nothing downstream will look at the diff.
    agent: claude
    prompt: implementer
    model: $MODEL
    session: true
    loop: 2
    on_pass: test

  - id: test
    description: The mechanical verdict on this change, as an exit code.
    run: npm test
    timeout: 5m
    on_pass: done
    on_fail: implement
EOF
} > impl_fast.yml
{ echo "$HDR"; cat <<EOF

version: "1.0"

description: >-
  Test first, one criterion at a time, for work whose criteria are already
  testable — money, auth, anything where a missed edge case costs.

task_template: default

steps:
  - id: implement
    description: >-
      Satisfy the acceptance criteria test first, one criterion at a time:
      a failing test, the least code that passes it, then the refactor.
    agent: claude
    prompt: implementer
    model: $MODEL
    session: true
    loop: 2
    on_pass: review

  - id: review
    description: >-
      Check the diff against the acceptance criteria and project standards,
      and that every criterion has a test that would fail without it.
    agent: claude
    prompt: reviewer
    model: $MODEL
    session: true
    on_pass: test
    on_fail: implement

  - id: test
    description: The mechanical verdict on this change, as an exit code.
    run: npm test
    timeout: 5m
    on_pass: done
    on_fail: implement
EOF
} > impl_tdd.yml
cd "$REPO"

# --- config: two Claude slots -------------------------------------------------
"$SPOOLWAY" config set unattended.blocked_model "$MODEL" >/dev/null
"$SPOOLWAY" config set agents.claude.concurrency 2 >/dev/null
# spoolway's shipped price table has no Haiku 5.5 yet. Without this table its
# cost and context show as unpriced. `config set` cannot create the table, so
# append it; the sync below tidies the comments around it. USD per 1M tokens,
# for prompts up to 100K; the cache rates are the usual multiples of input.
cat >> .spoolway/config.toml <<EOF

[models."$MODEL"]
context_window = 1000000
input = 0.10
output = 0.50
cache_read = 0.01
cache_write_5m = 0.125
cache_write_1h = 0.20
EOF
# The bugfix pipeline is gone, so its reproducer prompt would be unused.
rm -rf .spoolway/prompts/reproducer .spoolway/templates/tasks/bugfix.md
# `config set` leaves config.toml's comment block behind; sync rewrites it.
"$SPOOLWAY" sync >/dev/null
"$SPOOLWAY" pipeline check >/dev/null

# --- routines and the project job --------------------------------------------
R=.spoolway/routines
mkdir -p $R/nightly $R/weekly-deps $R/release-notes
cat > $R/nightly/audit-deps.md <<'EOF'
---
id: audit-deps
title: "chore(deps): audit the lockfile for advisories"
group: nightly
group_description: |
  The nightly hygiene pass: dependency advisories and documents that drifted from main.
pipeline: impl_fast
base: main
---
## Context

- `package-lock.json` pins every dependency the shop installs.
- `npm audit` reports known advisories against it.

## Intend

No known advisory sits in the lockfile overnight.

## Non-goals

- Major version bumps. Those belong to `weekly-deps`.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `npm audit --audit-level=moderate` exits 0, or every remaining advisory is listed in the report with why it stays.
- `npm test` passes.

## References

- `package-lock.json`
EOF
cat > $R/nightly/audit-docs.md <<'EOF'
---
id: audit-docs
title: "docs: bring the domain documents in line with main"
group: nightly
pipeline: impl_fast
base: main
depends_on:
  - audit-deps
---
## Context

- `README.md` and `docs/` describe the cart, checkout and auth modules.
- Lanes update documents per task, but a hand-merged change can skip that.

## Intend

Every document under `docs/` and the README describes the code on main as it is.

## Non-goals

- Changing code to match a document.
- anything not required by the acceptance criteria below

## Acceptance criteria

- Every exported function in `src/` that a document names still exists with that signature.
- Every statement about behaviour in `docs/` matches the code or is corrected.

## References

- `README.md`, `docs/`, `src/`
EOF
cat > $R/weekly-deps/bump-deps.md <<'EOF'
---
id: bump-deps
title: "chore(deps): bump outdated dependencies and keep the suite green"
group: weekly-deps
pipeline: impl
base: main
---
## Context

- `npm outdated` lists dependencies behind their latest release.
- Minor and patch bumps land weekly; majors need a person.

## Intend

Every dependency is on its latest minor and patch release.

## Non-goals

- Major version bumps.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `npm outdated` lists no dependency behind its latest minor or patch release.
- `npm test` passes.

## References

- `package.json`, `package-lock.json`
EOF
cat > $R/release-notes/draft-release-notes.md <<'EOF'
---
id: draft-release-notes
title: "docs(changelog): draft release notes from the merged work"
group: release-notes
pipeline: impl_fast
base: main
---
## Context

- `CHANGELOG.md` holds one section per release, newest first.
- Commit subjects follow Conventional Commits, so `git log` since the last tag groups cleanly.

## Intend

The next release has a drafted `CHANGELOG.md` section a person only has to approve.

## Non-goals

- Tagging or publishing anything.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `CHANGELOG.md` gains an `## Unreleased` section with `Added`, `Changed` and `Fixed` lists.
- Every `feat` and `fix` commit since the last tag appears in exactly one list.

## References

- `git log --oneline $(git describe --tags --abbrev=0 2>/dev/null || git rev-list --max-parents=0 HEAD)..HEAD`
EOF
cat > .spoolway/jobs.toml <<'EOF'
[jobs.nightly-audit]
schedule = "0 3 * * 1-5"   # weekdays at 03:00, local time
pipeline = "impl_fast"
routine  = "nightly"
EOF
for d in $R/*/; do "$SPOOLWAY" task contract --from "$d" >/dev/null; done

git add -A && git commit -qm "chore: shop skeleton, spoolway pipelines, routines and the nightly job"

# The per-user job lives in the project's home, not the repo.
mkdir -p "$HOME_DIR"
cat > "$HOME_DIR/jobs.toml" <<'EOF'
[jobs.weekly-deps]
schedule = "0 8 * * 1"     # Mondays at 08:00, local time
pipeline = "impl"
routine  = "weekly-deps"
EOF

# --- the two groups, pending and not queued ----------------------------------
P="$HOME_DIR/pending"
mkdir -p "$P"
cat > "$P/cart-empty-state.md" <<'EOF'
---
id: cart-empty-state
title: "feat(cart): show an empty state instead of a blank page"
group: cart
group_description: |
  Make the cart page useful: an empty state, real totals with tax and shipping,
  and discount codes on top of those totals.
pipeline: impl_fast
base: main
---
## Context

- `renderCart` in `src/render.js` returns an empty string for an empty cart.
- The page shell shows whatever `renderCart` returns, so an empty cart is a blank page.

## Intend

An empty cart tells the shopper it is empty and points them back to the catalogue.

## Non-goals

- Restyling the filled cart.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `renderCart` on an empty cart returns a paragraph reading "Your cart is empty."
- The empty state links to `/products` with the text "Keep shopping".
- `npm test` passes, with a test covering the empty state.

## References

- `src/render.js` — the cart page renderer.
- `test/cart.test.js` — the test style to follow.
EOF
cat > "$P/cart-totals.md" <<'EOF'
---
id: cart-totals
title: "feat(cart): total the cart with tax and shipping"
group: cart
pipeline: impl
base: main
depends_on:
  - cart-empty-state
---
## Context

- `subtotal` in `src/cart.js` sums price times quantity, in cents.
- The cart page shows only the subtotal; tax and shipping appear first at the payment provider.
- Money is kept in integer cents everywhere. Rounding happens once, on the tax line.

## Intend

The cart shows what the shopper will actually pay: subtotal, tax, shipping and total.

## Non-goals

- Discounts. `cart-discounts` builds on this.
- anything not required by the acceptance criteria below

## Acceptance criteria

- A new `totals(cart)` in `src/cart.js` returns `{ subtotal, tax, shipping, total }` in cents.
- Tax is 8% of the subtotal, rounded half up to the cent.
- Shipping is 499 cents, and 0 when the subtotal is 5000 cents or more.
- `renderCart` shows all four lines.
- `npm test` passes, with tests for the free-shipping boundary and the tax rounding.

## References

- `src/cart.js` — where `subtotal` lives.
- `src/render.js` — the cart page renderer.
EOF
cat > "$P/cart-discounts.md" <<'EOF'
---
id: cart-discounts
title: "feat(cart): apply a discount code to the total"
group: cart
pipeline: impl
base: main
depends_on:
  - cart-totals
---
## Context

- `totals(cart)` from `cart-totals` returns subtotal, tax, shipping and total in cents.
- Marketing hands out two codes today: `WELCOME10` (10% off) and `SHIPFREE` (free shipping).

## Intend

A shopper can enter one discount code, and the totals reflect it.

## Non-goals

- Stacking more than one code.
- Storing codes anywhere but a constant in `src/cart.js`.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `applyCode(cart, code)` stores at most one code on the cart; an unknown code throws.
- `WELCOME10` takes 10% off the subtotal before tax.
- `SHIPFREE` sets shipping to 0.
- `totals(cart)` returns a `discount` field in cents, and `renderCart` shows it when non-zero.
- `npm test` passes, with a test per code and one for the unknown code.

## References

- `src/cart.js` — `totals` and the cart shape.
EOF
cat > "$P/auth-verify.md" <<'EOF'
---
id: auth-verify
title: "feat(auth): verify the session before checkout"
group: checkout
group_description: |
  Take payment at checkout: only a signed-in shopper can check out, the order is
  charged, and a receipt goes out once the charge succeeds.
pipeline: impl
base: main
---
## Context

- `checkout` in `src/checkout.js` accepts any token, or none.
- `sessionFor(token)` in `src/auth.js` returns the session or `null`.
- Sessions never expire today.

## Intend

Only a shopper with a live session can place an order.

## Non-goals

- Password resets, sign-up, or any change to `login`.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `checkout` throws "not signed in" when the token has no session.
- A session older than 30 minutes counts as expired, and `checkout` throws "session expired".
- The order records the session's email as `customer`.
- `npm test` passes, with tests for a missing, an expired and a live session.

## References

- `src/auth.js` — sessions.
- `src/checkout.js` — the order shape.
EOF
cat > "$P/checkout-charge.md" <<'EOF'
---
id: checkout-charge
title: "feat(checkout): charge the order through the payment gateway"
group: checkout
pipeline: impl_tdd
base: main
depends_on:
  - auth-verify
---
## Context

- `checkout` records every order as `unpaid`.
- There is no payment code yet. The gateway is reached through one function so tests can replace it.

## Intend

Checkout charges the order total and records whether the charge went through.

## Non-goals

- Talking to a real payment provider. The gateway stays a function passed in.
- Refunds.
- anything not required by the acceptance criteria below

## Acceptance criteria

- `checkout(cart, { token, charge })` calls `charge({ amount, customer })` once.
- A charge that resolves marks the order `paid` and stores the charge id.
- A charge that throws marks the order `failed` and keeps the error message.
- `npm test` passes, with a fake gateway covering both outcomes.

## References

- `src/checkout.js` — the order shape.
EOF
cat > "$P/checkout-receipt.md" <<'EOF'
---
id: checkout-receipt
title: "feat(checkout): send a receipt once the charge succeeds"
group: checkout
pipeline: impl_fast
base: main
depends_on:
  - checkout-charge
---
## Context

- A paid order carries its lines, amount, customer and charge id.
- Mail is sent through one `send({ to, subject, text })` function, passed in like the gateway.

## Intend

A shopper whose order is paid gets a plain-text receipt by email.

## Non-goals

- HTML email.
- A receipt for a failed charge.
- anything not required by the acceptance criteria below

## Acceptance criteria

- A new `receiptText(order)` in `src/receipt.js` lists each line, the total and the order id.
- `checkout` calls `send` once for a paid order, to the customer, with subject "Your receipt".
- A failed charge sends nothing.
- `npm test` passes, with tests for both cases.

## References

- `src/checkout.js` — where the order is paid.
EOF
"$SPOOLWAY" task contract --from "$P" >/dev/null

# Trust the checkout in Claude Code, or every lane stops on the trust dialog.
python3 "$HERE/claude-trust.py" add "$REPO"

echo "$HOME_DIR"
