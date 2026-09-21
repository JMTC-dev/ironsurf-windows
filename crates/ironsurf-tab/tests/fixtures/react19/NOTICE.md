# React 19 fixture bundle

`reactapp.js` is a browser IIFE bundle of:

- `react@19.x`
- `react-dom@19.x` (client entry)

produced with `esbuild --bundle --minify --format=iife`, plus the small demo
component tree inlined from the same build (see the test file header for the
exact source). Both packages are MIT-licensed; their license headers are kept
inline by `--legal-comments=inline`.

Why it is committed: it is the ground truth for `tests/react_render.rs` — the
tab must run **real React**, not a hand-written approximation. Rebuild with:

```
npm install react@19 react-dom@19 esbuild
node_modules/.bin/esbuild app.jsx --bundle --minify --format=iife \
  --legal-comments=inline --outfile=reactapp.js
```

## Source of the bundled demo (`app.jsx`)

```jsx
import * as React from "react";
import { createRoot } from "react-dom/client";
const e = React.createElement;

function Measured() {
  const ref = React.useRef(null);
  const [w, setW] = React.useState(-1);
  React.useLayoutEffect(() => {
    const r = ref.current.getBoundingClientRect();
    setW(Math.round(r.width) + "x" + Math.round(r.height) + "/" + ref.current.offsetWidth);
  }, []);
  return e("div", { id: "measured", ref, style: { width: "123px", height: "20px" } }, String(w));
}

function App() {
  const [n, setN] = React.useState(0);
  const [items, setItems] = React.useState(["a", "b"]);
  const [iw, setIw] = React.useState(window.innerWidth);
  const firstIw = React.useRef(true);
  React.useEffect(() => {
    const onResize = () => setIw(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  React.useEffect(() => {
    if (firstIw.current) { firstIw.current = false; return; }
    document.title = "RESIZED:" + iw;
  }, [iw]);
  React.useEffect(() => {
    const btn = document.getElementById("btn");
    let clicks = 0;
    const t = setInterval(() => {
      btn.click();
      if (++clicks === 2) {
        clearInterval(t);
        setItems(["a", "b", "c"]);
        setTimeout(() => {
          document.title = "REACT-OK:geo=" + document.getElementById("measured").textContent
            + ";n=" + document.getElementById("count").textContent
            + ";iw=" + document.getElementById("iw").textContent
            + ";items=" + document.querySelectorAll("#list li").length
            + ";sym=" + (typeof Symbol === "function" && typeof Symbol.iterator === "symbol")
            + ";assign=" + (Object.assign({}, { a: 1 }).a === 1)
            + ";promise=" + (typeof Promise === "function");
        }, 250);
      }
    }, 100);
  }, []);
  return e("div", { id: "app" },
    e(Measured),
    e("p", { id: "count" }, String(n)),
    e("p", { id: "iw" }, String(iw)),
    e("ul", { id: "list" }, items.map((x) => e("li", { key: x }, x))),
    e("button", { id: "btn", onClick: () => setN((v) => v + 1) }, "inc"));
}
createRoot(document.getElementById("root")).render(e(App));
```
