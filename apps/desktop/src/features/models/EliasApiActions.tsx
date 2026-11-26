import { api } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { useMessage } from "../../shared/ui/message";
import styles from "./CursorSettings.module.scss";

export function EliasApiActions() {
  const message = useMessage();
  const openKeys = async () => {
    try {
      await api.openExternalUrl("https://subapi.elias.kdns.fr/keys");
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    }
  };

  return <div className={styles.endpointActions}>
    <Button size="small" onClick={() => void openKeys()}>{t("获取密钥")}</Button>
  </div>;
}
