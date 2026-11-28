`poll-countries.tsv` is derived from the `alpha_2` and `common_name`/`name`
fields of `iso_3166-1.json` in iso-codes 4.20.1 (Fedora package
`iso-codes-4.20.1-4.fc45`). The records are sorted by alpha-2 code; other
fields are omitted.

Upstream source and contributors: <https://salsa.debian.org/iso-codes-team/iso-codes>.
License: LGPL-2.1-or-later; see [iso-codes-LICENSE.txt](iso-codes-LICENSE.txt).

The compiled list keeps country selection available offline. System
`iso_3166-1` gettext translations are used when available; otherwise the
upstream English names are displayed. Incoming unknown codes are retained
and displayed as received; new polls can select only known codes.
