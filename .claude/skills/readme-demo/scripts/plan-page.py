#!/usr/bin/env python3
"""Fills spoolway-plan's template.html with the demo's cart plan, the page
`/spoolway-plan` would write for the cart group, so the README's plan
screenshot shows the template as it ships today.

Usage: plan-page.py TEMPLATE OUT     Prints OUT.
"""
import sys
from pathlib import Path

template, out = Path(sys.argv[1]), Path(sys.argv[2])
html = template.read_text()
head, rest = html.split("<body>", 1)
rail_end = rest.index("    <nav>")
rail_top = rest[:rail_end]
script_start = rest.index("<script>\n// The only script")
tail = rest[script_start:]

head = head.replace("[[plan-slug]]", "cart")
rail_top = rail_top.replace("[[plan-slug]]", "cart")

decisions = [
    ("cents-one-rounding", "Cents, one rounding",
     "Money stays in integer cents, and only tax is rounded",
     "Every price in the shop is already an integer number of cents. Rounding once, on the tax "
     "line, keeps the four totals adding up exactly.",
     "subtotal 2400 → tax 192 → shipping 499 → total 3091",
     "totals(cart) returns subtotal, tax, shipping and total in cents."),
    ("free-shipping-from-5000", "Free shipping from 5000",
     "Shipping is free from a 5000-cent subtotal",
     "Marketing promises free shipping on orders of $50 and more. The line sits on the subtotal "
     "before tax, so a discount can drop an order below it.",
     "4999 → shipping 499    5000 → shipping 0",
     "Shipping is 499 cents below the line and 0 from it."),
    ("one-code-before-tax", "One code, before tax",
     "One discount code per cart, taken off before tax",
     "Two codes exist today, WELCOME10 and SHIPFREE. Stacking them is out of scope, and taking a "
     "code off before tax matches what the payment provider charges.",
     "subtotal − discount → tax → shipping → total",
     "applyCode(cart, code) stores at most one code, and totals(cart) gains a discount field."),
]
steps = [
    ("empty-cart", "Empty cart", "$ open /cart with nothing in it",
     "Your cart is empty.\nKeep shopping → /products"),
    ("totals-shown", "Totals shown", "$ open /cart with two mugs",
     "Mug × 2 — $24.00\n\nSubtotal   $24.00\nTax         $1.92\nShipping    $4.99\nTotal      $30.91"),
    ("code-applied", "Code applied", "$ apply WELCOME10",
     "Mug × 2 — $24.00\n\nSubtotal   $24.00\nDiscount   −$2.40\nTax         $1.73\nShipping    $4.99\nTotal      $28.32"),
]

nav = ['    <nav>', '      <p class="eyebrow">On this page</p>', '      <ul class="index">',
       '        <li><a href="#context">Context</a></li>', '        <li><a href="#decisions">Decisions</a>',
       '          <ul>']
nav += [f'            <li><a href="#d-{s}">{t}</a></li>' for s, t, *_ in decisions]
nav += ['          </ul>', '        </li>', '        <li><a href="#mockup">Mockup</a>', '          <ul>']
nav += [f'            <li><a href="#m-{s}">{t}</a></li>' for s, t, *_ in steps]
nav += ['          </ul>', '        </li>', '      </ul>', '    </nav>', '  </aside>', '']

tree = """├── src/
│   ├── cart.js        <span class="note">the cart shape and subtotal, in cents</span>
│   ├── checkout.js    <span class="note">the order, which records the subtotal</span>
│   └── render.js      <span class="note">renderCart, the cart page body, and money</span>
└── test/
    └── cart.test.js   <span class="note">the test style to follow</span>"""

main = ['  <main class="doc">', '', '    <h1>cart</h1>', '',
        '    <p class="tagline">Empty state, totals and discount codes</p>', '',
        '    <p class="where">',
        f'      <span class="file">{out}</span>',
        '    </p>', '', '    <h2 id="context">Context</h2>', '',
        '    <p class="standfirst">The cart page tells a shopper when their cart is empty, and '
        'otherwise shows what they will pay, with one discount code taken off. Today an empty cart '
        'renders as a blank page, and a filled one shows only its subtotal.</p>', '',
        f'    <pre class="tree">{tree}</pre>', '',
        '    <h2 id="decisions">Decisions</h2>',
        '    <p>Three rules for the money on the cart page.</p>', '']
for slug, _, title, why, figure, change in decisions:
    main += [f'    <section class="dec" id="d-{slug}">', f'      <h3>{title}</h3>', f'      <p>{why}</p>',
             '      <figure>', '        <div class="frame">',
             f'          <svg viewBox="0 0 800 120" role="img" aria-label="{figure}">',
             f'            <text x="40" y="68" font-family="ui-monospace, monospace" font-size="22" fill="currentColor">{figure}</text>',
             '          </svg>', '        </div>', '      </figure>', '',
             f'      <p class="change">{change}</p>', '    </section>', '']
main += ['    <h2 id="mockup">Mockup</h2>', '    <div class="mockup">', '']
for n, (slug, title, bar, body) in enumerate(steps, 1):
    main += [f'      <section class="mock-step" id="m-{slug}">', f'        <h3>{title}</h3>',
             '        <figure>', '          <div class="mocks">', '            <div class="mock">',
             f'              <div class="bar"><span class="step">{n}</span><span>{bar}</span></div>',
             f'<pre>{body}</pre>', '            </div>', '          </div>', '        </figure>',
             '      </section>', '']
main += ['    </div>', '', '  </main>', '</div>', '']

markdown = ["# cart", "", "Empty state, totals and discount codes", "", "## Context", "",
            "The cart page tells a shopper when their cart is empty, and otherwise shows what they "
            "will pay, with one discount code taken off.", "", "## Decisions", ""]
for _, _, title, why, _, change in decisions:
    markdown += [f"### {title}", "", why, "", change, ""]
markdown += ["## Mockup", ""] + [f"### {t}\n\n{b}\n" for _, t, _, b in steps]
plan_md = '<script type="text/markdown" id="plan">\n' + "\n".join(markdown) + "\n</script>\n\n"

out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(head + "<body>" + rail_top + "\n".join(nav) + "\n" + "\n".join(main) + "\n" + plan_md + tail)
assert "[[" not in out.read_text(), "a placeholder is left"
print(out)
