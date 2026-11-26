import type { ModelType } from "../../shared/api";
import { matchesPresetUrl, modelPresets, type ModelPreset } from "../../shared/utils/modelPresets";
import styles from "./CursorPresetChips.module.scss";

export function CursorPresetChips({ type, baseUrl, onPick }: { type: ModelType; baseUrl: string; onPick: (preset: ModelPreset) => void }) {
  return <div className={styles.wrap}>
    <span className={styles.label}>{t("常用预设")}</span>
    <div className={styles.chips}>
      {modelPresets.map((preset) => {
        const active = matchesPresetUrl(preset, type, baseUrl);
        return <button
          type="button"
          aria-pressed={active}
          key={preset.key}
          className={active ? `${styles.chip} ${styles.active}` : styles.chip}
          title={preset.keyHint}
          onClick={() => onPick(preset)}
        >
          <img className={styles.icon} src={preset.icon} alt="" />
          {preset.name}
        </button>;
      })}
    </div>
  </div>;
}
