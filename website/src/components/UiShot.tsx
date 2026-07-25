import type {ReactNode} from 'react';
import ThemedImage from '@theme/ThemedImage';
import useBaseUrl from '@docusaurus/useBaseUrl';

import styles from './UiShot.module.css';

/**
 * A screenshot of the admin web UI, captured by `screenshots/capture.mjs`.
 *
 * Every shot exists in a light and a dark variant; the one matching the
 * reader's docs theme is served, so a dark screenshot never lands in a light
 * page. `name` is the base filename in static/img/ui/ without the theme suffix
 * (e.g. "policy-graph" -> policy-graph-light.png / policy-graph-dark.png).
 */
export default function UiShot({
  name,
  alt,
  caption,
}: {
  name: string;
  alt: string;
  caption?: ReactNode;
}): ReactNode {
  return (
    <figure className={styles.figure}>
      <ThemedImage
        alt={alt}
        sources={{
          light: useBaseUrl(`/img/ui/${name}-light.png`),
          dark: useBaseUrl(`/img/ui/${name}-dark.png`),
        }}
      />
      {caption && <figcaption className={styles.caption}>{caption}</figcaption>}
    </figure>
  );
}
