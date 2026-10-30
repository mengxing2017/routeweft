# Sources and distribution

Request coordination and world-copy handling in src/index.ts are adapted from:
- https://github.com/Project-OSRM/osrm-frontend/tree/ab85f4fb05e762339475b8771a30622e9856c02f
- src/router_patches.js (longitude wrapping, world projection, abort/stale response handling)
- src/route_throttle.js (preview backoff: 4 seconds, doubling to 30 seconds after repeated failure)

Copyright (c) 2014, Project OSRM. Complete BSD-2-Clause terms are retained in
LICENSE.osrm-frontend and included in the npm archive. Preserve them in downstream distributions.

Changes: TypeScript, transport injection, Promise outcomes, explicit cancellation,
local RouteWeft contracts, semantic errors excluded from backoff, no Leaflet mutation.
OSRM wire protocol, geocoding, demo endpoints and UI are not included.
Original RouteWeft code has no project-wide license declared yet. Technical packaging
does not grant additional rights or authorize public registry publication.
