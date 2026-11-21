import {
  Link,
  Outlet,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  lazyRouteComponent,
} from "@tanstack/react-router";
import { Home } from "./screens/Home.tsx";
import { strings } from "./strings.ts";

function Layout() {
  return (
    <div className="layout">
      <header className="header">
        <h1>{strings.app.name}</h1>
        <p className="tagline">{strings.app.tagline}</p>
        <nav aria-label={strings.nav.main}>
          <Link to="/" activeOptions={{ exact: true }}>
            {strings.nav.home}
          </Link>
          {__SWITCHYARD_DEVTOOLS__ && <Link to="/dev/load">{strings.nav.loadCheck}</Link>}
        </nav>
      </header>
      <main>
        <Outlet />
      </main>
    </div>
  );
}

const root = createRootRoute({ component: Layout });
const home = createRoute({ getParentRoute: () => root, path: "/", component: Home });
// Loaded on demand so xterm.js stays out of the main bundle.
const loadCheck = createRoute({
  getParentRoute: () => root,
  path: "/dev/load",
  component: lazyRouteComponent(() => import("./screens/LoadCheck.tsx"), "LoadCheck"),
});

// Memory history: the webview has no address bar; deep links (notification
// clicks, the command palette) navigate by route, e.g. /workspace/$id later.
export const router = createRouter({
  routeTree: root.addChildren(__SWITCHYARD_DEVTOOLS__ ? [home, loadCheck] : [home]),
  history: createMemoryHistory({ initialEntries: ["/"] }),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
