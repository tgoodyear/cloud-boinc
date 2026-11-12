# Dashboard feed

A project can show figures that are not in the BOINC tables, such as pages fetched, pages indexed
or accuracy, as panels on `work.php`. It publishes a JSON document and names its address in the
config:

```xml
<dashboard_feed_url>https://example.org/boinc-dashboard.json</dashboard_feed_url>
```

The feed has to be on a hostname other than the project's own: a request from the Worker to a
path on the hostname it is serving returns a 522 (it goes to the origin, not to the Workers on the
route), and the panels show the note instead. The address must be `https`, at most 300 characters, with no credentials in it. The config is
refused otherwise. Without the tag the page has no panels.

## How it is fetched

The web Worker fetches the document when it builds `work.php`, which is about once a minute
for each data center, because the page is cached for that long. The request is a `GET` with a 3-second limit for
the whole exchange, redirects are not followed, a status other than 200 is a failure, and at most
64 KB is read. The document is then checked against the schema below. If any of that fails, the
panels are replaced by a note saying the project's figures are not available; the rest of the page
is unaffected. The Worker logs the reason, not the address.

Nothing from the document is shown unescaped, and the Worker sends nothing but the request.

## Schema

The checks are strict: an unknown field, a wrong type, a value out of range or a text over its
length refuses the whole document.

```json
{
  "schema": 1,
  "updated": 1800000000,
  "sections": [
    {
      "title": "Corpus",
      "updated": 1800000000,
      "stale_after_secs": 3600,
      "link": { "label": "Full dashboard", "url": "https://example.org/dashboard" },
      "figures": [
        { "label": "Pages fetched", "value": 1234567, "unit": "pages", "note": "since June" },
        { "label": "Accuracy", "value": null, "note": "not measured yet" },
        { "label": "Mode", "value": "catching up" }
      ],
      "series": [
        {
          "title": "Pages fetched per day",
          "unit": "pages",
          "kind": "bars",
          "start": 1799200000,
          "interval_secs": 86400,
          "values": [1200, 1500, null, 1800]
        }
      ]
    }
  ]
}
```

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `schema` | integer | yes | Must be 1 |
| `updated` | integer | no | When the figures were made, Unix seconds; the default for each section |
| `sections` | array | yes | At most 8, each shown as a card |
| `title` | string | yes | Card title, at most 80 characters |
| `updated` (section) | integer | no | When this section's figures were made |
| `stale_after_secs` | integer | no | Age after which the badge says "stale"; 60 to a year; default 3600 |
| `link` | object | no | `label` (at most 60 characters) and `url`, which must be `https` |
| `figures` | array | no | At most 12 big numbers |
| `label` | string | yes | At most 60 characters |
| `value` | number, string or null | no | A finite number (magnitude at most 1e18) is shown with separators; a string of at most 40 characters is shown as it is; null, an empty string or no `value` is shown as "unknown" |
| `unit` | string | no | At most 24 characters, shown under the figure |
| `note` | string | no | At most 160 characters, shown under the figure |
| `series` | array | no | At most 3 small charts |
| `kind` | string | no | `line` (default), `bars` or `area` |
| `start` | integer | yes | Unix time of the first value |
| `interval_secs` | integer | yes | Time between values, 60 seconds to 31 days |
| `values` | array | yes | 1 to 200 numbers; null leaves a gap and is shown as "unknown" |

Texts may not contain control characters. `updated` and `start` must be after September 2001, and
a series must end no later than a day after the time the page is built. A `series` is shown with a
table of its values, for screen readers and for reading off the numbers.

A figure whose value is unknown is shown as "unknown", never as 0; send 0 when the value is 0.

## Staleness

Each card has a badge. With `updated` it says how long ago, in green while that is within
`stale_after_secs` and in amber ("stale") after. Without `updated` it says the update time is
unknown. The project should republish at least as often as `stale_after_secs`; a document that is
no longer being updated stays on the page, marked stale, and does not disappear.
