# Déploiement durci de lg-hue-sync sur la TV LG G4 (webOS 11)

Ce dossier prépare un déploiement « au moindre privilège » pour une seule TV :
LG OLED G4, webOS 11.2.0, noyau 6.12, ARMv7, rootée, systemd 255, cgroups v1.
Rien ici n'est encore appliqué à la TV. On déroule les étapes une par une, dans l'ordre.

## Le problème à corriger (P0)

Aujourd'hui, le démon tourne en root, avec toutes les capacités du noyau.
Il écrit `config.json`, ses fichiers temporaires et son verrou dans `/var/home/root/lg-hue-sync`.
Ce même dossier contient des fichiers que root exécute : l'unité systemd, `log-maintenance.sh`, `provision_luna.sh`.
Un démon compromis pourrait donc réécrire ces fichiers, et root les exécuterait au démarrage suivant.

La correction :

1. Le démon écrit seulement dans un sous-dossier `state/`.
2. Une règle systemd (`ProtectSystem=strict`) rend tout le reste du disque en lecture seule pour lui.

## Contenu du dossier

| Fichier | Destination sur la TV | Rôle |
|---|---|---|
| `lg-hue-sync.service` | `/var/home/root/lg-hue-sync/` | Unité de base, avec `state/` |
| `lg-hue-sync.service.d/*.conf` | `/var/home/root/lg-hue-sync/lg-hue-sync.service.d/` | Une couche par fichier : protections (10 à 60), chien de garde (70) |
| `05-lg-hue-sync` | `/var/lib/webosbrew/init.d/` | Script de démarrage (Homebrew Channel) |
| `lg-hue-sync-log.service`, `.timer` | `/var/home/root/lg-hue-sync/` | Vidage du journal à 1 Mio, chaque minute |
| `log-maintenance.sh` | `/var/home/root/lg-hue-sync/` | Script de vidage, chemin `state/daemon.log` |
| `luna/*.draft.json` | (brouillons) | Droits Luna réduits, à compléter à l'étape 3 |

Le script de démarrage `55-lg-hue-sync-log` existant ne change pas.
Il copie déjà `lg-hue-sync-log.service` et `.timer` depuis le dossier d'installation.

Chaque couche se retire seule : on supprime son fichier `.conf`, on relance le script de démarrage, on redémarre le service.
Tout ce que le script copie va dans `/run`, qui est vidé à chaque redémarrage de la TV.

## Avant de commencer : variables et petites fonctions

Ouvrir un shell root sur la TV.
Coller ce bloc au début de **chaque** session.
`J` est la date de l'étape 0 : si on reprend un autre jour, remettre la même valeur à la main (par exemple `J=20261030`).

```sh
D=/var/home/root/lg-hue-sync
H=/var/lib/webosbrew/init.d
S=/tmp/lhs-webos11
J=$(date +%Y%m%d)

# État du démon (sans afficher de secret).
lhs_status() {
    { curl -s http://127.0.0.1:8088/api/status 2>/dev/null || wget -qO- http://127.0.0.1:8088/api/status; } \
      | grep -oE '"(is_syncing|tv_power_state|fps|light_update_fps|hue_connected|capture_hardware|capture_resolution)":[^,]*'
}

# Capacités, NoNewPrivs et seccomp du processus principal d'une unité.
lhs_proc() {
    P=$(systemctl show -p MainPID --value "$1")
    echo "PID=$P"
    grep -E 'CapEff|CapBnd|CapAmb|NoNewPrivs|Seccomp' "/proc/$P/status"
}

# Lance le démon en unité temporaire « lhs-test », avec les réglages des fichiers .conf donnés.
lhs_test() {
    n=$#
    for f in "$@"; do
        [ -f "$f" ] || continue
        while IFS= read -r l || [ -n "$l" ]; do
            case $l in ''|'#'*|'['*) continue ;; esac
            set -- "$@" -p "$l"
        done < "$f"
    done
    shift "$n"
    systemd-run --unit=lhs-test \
        -p WorkingDirectory=$D/state \
        -p StandardOutput=append:$D/state/daemon.log \
        -p StandardError=inherit \
        "$@" \
        $D/lg-hue-sync run --config $D/state/config.json
}

# Arrête et oublie l'unité temporaire.
lhs_test_end() {
    systemctl stop lhs-test 2>/dev/null
    systemctl reset-failed lhs-test 2>/dev/null
    true
}
```

Copier ce dossier sur la TV.
La session principale lance cette commande depuis le Mac, à la racine du dépôt :

```sh
scp -O -r deploy/webos11 root@<IP-de-la-TV>:/tmp/lhs-webos11
```

`/tmp` est vidé au redémarrage de la TV : recopier le dossier si besoin.

## Contrôles communs

On y renvoie à chaque étape.

- **Service actif** : `systemctl is-active lg-hue-sync` affiche `active`.
- **Capture** : `lhs_status` affiche `"capture_hardware":true` et un `"fps"` au-dessus de 0, TV allumée sur une image.
- **Hue** : `lhs_status` affiche `"hue_connected":true`, et les lampes suivent l'image.
- **Requêtes Luna** : `lhs_status` affiche `"tv_power_state":"active"` TV allumée.
- **Extinction en veille** : mettre la TV en veille avec la télécommande. Les lampes s'éteignent en moins de 10 secondes.
  Rallumer la TV : le service et la synchro reviennent comme avant.
- **Erreurs** : cette commande ne doit montrer aucune ligne nouvelle.

  ```sh
  grep -iE 'Operation not permitted|Permission denied|Read-only file system|denied|EPERM|EROFS' $D/state/daemon.log | tail -20
  ```

En cas d'échec d'une unité, lire son code de sortie (226 = espace de noms, 218 = capacités, 228 = seccomp) :

```sh
systemctl show lg-hue-sync -p Result -p ExecMainStatus
tail -50 $D/state/daemon.log
```

## Étape 0 : sauvegardes datées et relevés (aucun changement)

**Sauvegarder** chaque fichier touché, avec le suffixe `.bak-AAAAMMJJ`.
Une sauvegarde existante du même jour n'est pas écrasée.

```sh
for f in $D/lg-hue-sync.service $D/config.json $D/config.json.lock $D/daemon.log \
         $D/log-maintenance.sh $D/lg-hue-sync-log.service $D/lg-hue-sync-log.timer \
         $H/05-lg-hue-sync $H/55-lg-hue-sync-log \
         /var/luna-service2/roles.d/org.webosbrew.lg-hue-sync.json \
         /var/luna-service2/client-permissions.d/org.webosbrew.lg-hue-sync.json \
         /var/luna-service2-dev/roles.d/org.webosbrew.lg-hue-sync.json \
         /var/luna-service2-dev/client-permissions.d/org.webosbrew.lg-hue-sync.json; do
    if [ ! -e "$f" ]; then echo "ABSENT $f"
    elif [ -e "$f.bak-$J" ]; then echo "DÉJÀ $f.bak-$J"
    else cp -p "$f" "$f.bak-$J" && echo "OK $f.bak-$J"; fi
done
# Les sauvegardes des scripts de démarrage ne doivent jamais s'exécuter au boot.
chmod 644 $H/*.bak-$J
```

`config.json.bak-…` contient la clé d'appairage Hue : ne pas l'afficher, ne pas la copier hors de la TV.

**Relever** ces faits. Ils décident de certains réglages des étapes suivantes.

```sh
systemctl --version | tail -1          # doit contenir +SECCOMP (couches 30, 40, 60)
readlink -f /home /root                # un résultat sous /var/home interdit ProtectHome
ls -l /dev/video60                     # majeur 81 = « char-video4linux » utilisable
ls /sys/fs/cgroup                      # noms exacts des dossiers cpu,cpuacct et memory
cat /sys/block/*/queue/scheduler       # « [none] » partout = IOScheduling* sans effet
ls -la /tmp                            # sockets ou fichiers partagés par Luna ou pmlog ?
command -v curl wget ionice            # outils présents pour les contrôles
systemd-analyze security lg-hue-sync | tail -1    # note d'exposition de départ
```

Faire aussi les **contrôles communs** une fois, pour connaître l'état de départ.

**Retour arrière** : rien à défaire. Pour effacer les sauvegardes plus tard : `rm $D/*.bak-$J $H/*.bak-$J`.

## Étape 1 : séparer `state/` (nouvelle unité et nouveau script de démarrage)

Risque : faible. Le démon garde son appairage, car on déplace `config.json` sans le modifier.

```sh
systemctl stop lg-hue-sync
[ -d $D/state ] || mkdir -m 700 $D/state
mv $D/config.json $D/state/config.json
[ ! -e $D/config.json.lock ] || mv $D/config.json.lock $D/state/config.json.lock
[ ! -e $D/daemon.log ] || mv $D/daemon.log $D/state/daemon.log
rm -f $D/config.new-*
cp $S/lg-hue-sync.service $D/lg-hue-sync.service && chmod 644 $D/lg-hue-sync.service
mkdir -p $D/lg-hue-sync.service.d
cp $S/log-maintenance.sh $D/log-maintenance.sh && chmod 755 $D/log-maintenance.sh
cp $S/lg-hue-sync-log.service $S/lg-hue-sync-log.timer $D/ && chmod 644 $D/lg-hue-sync-log.service $D/lg-hue-sync-log.timer
cp $S/05-lg-hue-sync $H/05-lg-hue-sync && chmod 755 $H/05-lg-hue-sync
systemd-analyze verify $D/lg-hue-sync.service $D/lg-hue-sync-log.service
$H/05-lg-hue-sync
$H/55-lg-hue-sync-log
```

`systemd-analyze verify` peut signaler des unités absentes. Seules les lignes « Unknown key » ou « Invalid » sont bloquantes.

**Contrôles**

```sh
systemctl show lg-hue-sync -p ExecStart -p WorkingDirectory -p StandardOutput -p Restart -p RestartUSec \
  -p StartLimitIntervalUSec -p StartLimitBurst -p TimeoutStopUSec
P=$(systemctl show -p MainPID --value lg-hue-sync); tr '\0' ' ' < /proc/$P/cmdline; echo   # binaire direct, pas de « sh -c »
ls -la $D/state                        # config.json, config.json.lock, daemon.log qui grossit
[ ! -e $D/config.json ] && echo "OK : pas de nouveau config.json à l'ancien endroit"
systemctl start lg-hue-sync-log.service; systemctl show lg-hue-sync-log.service -p Result   # Result=success
systemctl is-active lg-hue-sync-log.timer
```

Puis les **contrôles communs**, dont l'extinction en veille.
Vérifier aussi que « Redémarrer » dans l'interface web laisse le service actif. L'unité garde `Restart=always` : après un appairage Hue ou Nanoleaf, le démon s'arrête volontairement avec un code de succès et compte sur systemd pour le relancer. La limite de démarrages (10 en 30 minutes) arrête quand même une boucle de plantages.

**Retour arrière** (une ligne) :

```sh
systemctl stop lg-hue-sync; cp -p $D/state/config.json $D/config.json; for f in $D/lg-hue-sync.service $D/log-maintenance.sh $D/lg-hue-sync-log.service $D/lg-hue-sync-log.timer $H/05-lg-hue-sync; do cp -p $f.bak-$J $f; done; chmod 755 $H/05-lg-hue-sync; rm -rf /run/systemd/system/lg-hue-sync.service.d; $H/05-lg-hue-sync; $H/55-lg-hue-sync-log
```

On recopie le `config.json` actuel, pas la sauvegarde : un appairage refait entre-temps est gardé.

## Étape 2 : les couches de protection, une par une

Ordre, du moins risqué au plus risqué : **10 → 20 → 50 → 40 → 30 → 60**.
La couche 50 passe avant la 40, car `ProtectClock=` (couche 40) active aussi la liste blanche des périphériques.

Pour chaque couche `X` (par exemple `X=10-resources`), suivre ces quatre temps.

**a. Essai en unité temporaire.** Les couches déjà installées sont reprises, plus la nouvelle.

```sh
X=10-resources
systemctl stop lg-hue-sync
lhs_test $D/lg-hue-sync.service.d/*.conf $S/lg-hue-sync.service.d/$X.conf
sleep 20
systemctl is-active lhs-test
lhs_proc lhs-test
lhs_status
```

Faire les contrôles propres à la couche (ci-dessous) avec `U=lhs-test`.
Si `systemd-run` refuse un réglage (« Unknown assignment »), le noter et passer directement au temps c.

**b. Fin de l'essai.**

```sh
lhs_test_end
```

Si l'essai a échoué, chercher la ligne fautive : copier le fichier, retirer une ligne, relancer l'essai.

```sh
grep -v '^ProtectClock' $S/lg-hue-sync.service.d/$X.conf > /tmp/$X.conf
lhs_test $D/lg-hue-sync.service.d/*.conf /tmp/$X.conf
```

Puis `lhs_test_end` et `systemctl start lg-hue-sync`.

**c. Installation comme fichier permanent.**

```sh
cp $S/lg-hue-sync.service.d/$X.conf $D/lg-hue-sync.service.d/ && chmod 644 $D/lg-hue-sync.service.d/$X.conf
$H/05-lg-hue-sync && systemctl restart lg-hue-sync
```

**d. Contrôles** : les contrôles propres à la couche avec `U=lg-hue-sync`, puis tous les **contrôles communs**, dont l'extinction en veille.
L'extinction en veille ne se teste qu'ici : l'unité temporaire n'a pas `Conflicts=sleep.target`.
Suivre la note d'exposition : `systemd-analyze security lg-hue-sync | tail -1` doit baisser.

**Retour arrière d'une couche** (une ligne) :

```sh
rm $D/lg-hue-sync.service.d/$X.conf && $H/05-lg-hue-sync && systemctl restart lg-hue-sync
```

### 10-resources : CPU, mémoire, priorités

Limite : 60 % d'un cœur, 96 Mio de mémoire, nice 10, priorité disque basse, 512 fichiers ouverts, pas de fichier core.
Sur cgroups v1, `MemoryHigh=` n'a aucun effet ; `MemoryMax=` devient `memory.limit_in_bytes` ; `CPUWeight=` devient `cpu.shares`.

```sh
systemctl show $U -p CPUQuotaPerSecUSec -p CPUWeight -p MemoryMax -p Nice -p OOMScoreAdjust -p LimitNOFILE -p LimitCORE
CG=$(systemctl show -p ControlGroup --value $U); echo $CG
cat /sys/fs/cgroup/cpu,cpuacct$CG/cpu.cfs_quota_us      # 60000
cat /sys/fs/cgroup/cpu,cpuacct$CG/cpu.shares            # 512
cat /sys/fs/cgroup/memory$CG/memory.limit_in_bytes      # 100663296
grep nr_throttled /sys/fs/cgroup/cpu,cpuacct$CG/cpu.stat
P=$(systemctl show -p MainPID --value $U); cat /proc/$P/oom_score_adj; awk '{print "nice=" $19}' /proc/$P/stat
grep -E 'open files|core file' /proc/$P/limits
```

Si le dossier cgroup ne s'appelle pas `cpu,cpuacct`, prendre le nom relevé à l'étape 0.
Le `fps` de `lhs_status` doit rester proche de celui de l'étape 0.
Si `nr_throttled` grimpe vite et que le fps baisse, monter `CPUQuota=` (par exemple à 80 %).

### 20-filesystem : écriture seulement dans `state/` (ferme le P0)

Tout le disque devient en lecture seule pour le démon, sauf `state/`. `/tmp` devient privé.

```sh
systemctl show $U -p ProtectSystem -p ReadWritePaths -p PrivateTmp -p UMask -p ProtectProc
systemd-run --wait --pipe -p ProtectSystem=strict -p ReadWritePaths=$D/state /bin/sh -c \
  "touch $D/probe; touch $D/state/probe && rm $D/state/probe && echo ok"
# Attendu : « Read-only file system » pour $D/probe, puis « ok ».
rm -f $D/probe
```

Dans l'interface web, changer un réglage et l'enregistrer.
Puis `stat -c '%y' $D/state/config.json` doit montrer l'heure du moment.
`ProtectHome=` reste absent : l'ajouter seulement si `readlink -f /home /root` ne pointe pas sous `/var/home`.

### 50-devices : seulement `/dev/video60`

```sh
systemctl show $U -p DevicePolicy -p DeviceAllow
CG=$(systemctl show -p ControlGroup --value $U); cat /sys/fs/cgroup/devices$CG/devices.list
```

La liste montre `/dev/video60` en `rw` et les pseudo-périphériques (`/dev/null`, `/dev/zero`, `/dev/urandom`…).
Rien à prévoir pour `/dev/shm` : c'est un tmpfs, pas un périphérique.
Point à surveiller : la capture doit démarrer. Un périphérique ouvert seulement à l'initialisation (par exemple `/dev/dma_heap/*`) serait bloqué.

### 40-kernel : pas de modification du noyau

```sh
systemctl show $U -p ProtectKernelTunables -p ProtectKernelModules -p ProtectKernelLogs -p ProtectControlGroups -p ProtectHostname -p ProtectClock -p DeviceAllow
```

`ProtectClock=yes` ajoute en silence `DeviceAllow=char-rtc r`. Le fichier ajoute donc aussi `/dev/video60`, sinon la capture tomberait.

### 30-privileges : aucune capacité

Le démon reste root (uid 0), mais sans aucune capacité du noyau.

```sh
lhs_proc $U
# Attendu : CapEff, CapBnd, CapAmb = 0000000000000000 ; NoNewPrivs = 1 ; Seccomp = 2
```

Risque principal : le pilote LG refuse un appel sur `/dev/video60` sans capacité.
Symptôme : `"capture_hardware":false`, ou une erreur de capture dans `state/daemon.log`.

### 60-syscalls : filtre d'appels système

```sh
lhs_proc $U
# Attendu : Seccomp = 2 et Seccomp_filters au-dessus de 0
```

Essai à blanc facultatif, avant le temps a : journaliser sans bloquer.

```sh
systemctl stop lg-hue-sync
systemd-run --unit=lhs-test -p SystemCallLog=~@system-service -p WorkingDirectory=$D/state \
  -p StandardOutput=append:$D/state/daemon.log -p StandardError=inherit \
  $D/lg-hue-sync run --config $D/state/config.json
sleep 30; dmesg | grep -iE 'seccomp|audit' | tail -20
lhs_test_end; systemctl start lg-hue-sync
```

Chaque ligne montre un appel hors de `@system-service` : l'ajouter à `60-syscalls.conf` s'il est légitime.
Si `dmesg` ne montre rien, le noyau ne journalise pas seccomp : passer au temps a.
Un appel système refusé renvoie EPERM : chercher « Operation not permitted » dans `state/daemon.log`.

### 70-watchdog : relance si le démon se fige

Cette couche ne restreint rien. Elle demande au démon de signaler qu'il vit :
- il envoie « prêt » une fois démarré, puis un signal toutes les 30 s ;
- sans signal pendant 60 s, systemd le tue et le relance.

Le code envoie déjà ces signaux (`sd_notify` dans `src/main.rs`) : il suffit de les activer. À poser en dernier, une fois les autres couches validées.

```sh
systemctl stop lg-hue-sync
systemd-run --unit=lhs-test -p Type=notify -p NotifyAccess=main -p WatchdogSec=60s \
  -p WorkingDirectory=$D/state -p StandardOutput=append:$D/state/daemon.log -p StandardError=inherit \
  $D/lg-hue-sync run --config $D/state/config.json
sleep 10; systemctl show -p Type,WatchdogUSec,ActiveState lhs-test
# Attendu : Type=notify, WatchdogUSec=1min, ActiveState=active
sleep 120; systemctl show -p NRestarts,ActiveState lhs-test
# Attendu : toujours active, sans redémarrage
lhs_test_end; systemctl start lg-hue-sync
```

Si le test passe, copier `70-watchdog.conf` dans `$D/lg-hue-sync.service.d/`, relancer le script de démarrage à la main, puis `systemctl restart lg-hue-sync`.

**Retour arrière** : supprimer `70-watchdog.conf` dans `$D/lg-hue-sync.service.d/` et dans `/run/systemd/system/lg-hue-sync.service.d/`, puis `systemctl daemon-reload && systemctl restart lg-hue-sync`.

## Étape 3 : réduire les droits Luna

Risque : le plus élevé. Une erreur coupe la capture, mais pas le reste de la TV.
Les requêtes de veille et d'état (tvpower, applicationManager, tv.display, videooutput, arccontroller) passent par `luna-send`.
`luna-send` a son propre rôle : cette étape ne les touche pas.

### 3a. Relever les noms et méthodes réellement utilisés par la capture

TV allumée sur une image, synchro active.

```sh
ls-monitor --help 2>&1 | head -40      # confirmer les options sur ce webOS
systemctl stop lg-hue-sync
ls-monitor > /tmp/lhs-lsmon.txt 2>&1 &
M=$!
sleep 2
systemctl start lg-hue-sync
sleep 30
kill $M
P=$(systemctl show -p MainPID --value lg-hue-sync); echo "PID=$P"
head -40 /tmp/lhs-lsmon.txt            # lire le format des lignes
grep -E 'com\.webos\.service\.capture|com\.webos\.rm|org\.webosbrew\.lg-hue-sync' /tmp/lhs-lsmon.txt > /tmp/lhs-lsmon-capture.txt
wc -l /tmp/lhs-lsmon-capture.txt
grep -oE '(com\.webos|org\.webosbrew)[A-Za-z0-9._*-]*' /tmp/lhs-lsmon-capture.txt | sort | uniq -c | sort -rn
grep -oE '/[A-Za-z0-9_]+(/[A-Za-z0-9_]+)*' /tmp/lhs-lsmon-capture.txt | sort | uniq -c | sort -rn | head -40
```

Si `--help` montre une option de liste des clients (souvent `-l`), la lancer et chercher le PID du démon :

```sh
ls-monitor -l 2>&1 | grep -E " $P |lg-hue-sync"
```

Noter pour la suite :

- les noms enregistrés par le démon (par exemple `com.webos.service.capture.client…`, `com.webos.rm.client…`, `org.webosbrew.lg-hue-sync…`) ;
- pour chaque nom, les services appelés (sortants) et les services qui le rappellent (entrants) ;
- chaque méthode appelée, sous la forme `service/catégorie/méthode`.

Pour voir aussi la reprise après veille, refaire la capture en mettant la TV en veille puis en la rallumant pendant les 30 secondes.

### 3b. Trouver les groupes de permissions

```sh
ls /usr/share/luna-service2/api-permissions.d/
grep -lE 'com\.webos\.service\.capture|com\.webos\.service\.capturepermission' /usr/share/luna-service2/api-permissions.d/*
```

Ouvrir ces fichiers. Pour chaque méthode notée en 3a, noter le nom du groupe qui la contient.
Faire de même pour le service de ressources vu en 3a (`com.webos.rm…` ou `com.webos.media…`).
Lire aussi `/usr/share/luna-service2/groups.d/` s'il existe : il peut lier les groupes à des niveaux de confiance.

### 3c. Remplir les brouillons

La session principale complète, dans le dépôt :

- `luna/role.draft.json` : remplacer chaque `__OUTBOUND_FROM_LS_MONITOR__` et `__INBOUND_FROM_LS_MONITOR__` par les services vus en 3a. Retirer un bloc entier si son nom n'apparaît pas en 3a.
- `luna/client-permissions.draft.json` : remplacer chaque `__GROUP_…__` par les groupes trouvés en 3b. Jamais `all`.

Elle enregistre le résultat sous `luna/role.json` et `luna/client-permissions.json`, puis le recopie dans `$S/luna/` sur la TV.

### 3d. Installer

```sh
grep -c '__' $S/luna/role.json $S/luna/client-permissions.json   # doit afficher 0 pour les deux
for R in /var/luna-service2 /var/luna-service2-dev; do
    cp $S/luna/role.json $R/roles.d/org.webosbrew.lg-hue-sync.json
    cp $S/luna/client-permissions.json $R/client-permissions.d/org.webosbrew.lg-hue-sync.json
done
ls-control scan-services
systemctl restart lg-hue-sync
```

**Contrôles** : tous les **contrôles communs**, surtout la capture, puis une veille et un réveil.
Si la TV garde un journal système (par exemple `/var/log/messages`), y chercher un refus Luna :

```sh
[ -f /var/log/messages ] && grep -iE 'lg-hue-sync|capture' /var/log/messages | grep -iE 'denied|permission|not allowed' | tail -20
```

**Retour arrière** (une ligne) :

```sh
for R in /var/luna-service2 /var/luna-service2-dev; do cp -p $R/roles.d/org.webosbrew.lg-hue-sync.json.bak-$J $R/roles.d/org.webosbrew.lg-hue-sync.json; cp -p $R/client-permissions.d/org.webosbrew.lg-hue-sync.json.bak-$J $R/client-permissions.d/org.webosbrew.lg-hue-sync.json; done; ls-control scan-services; systemctl restart lg-hue-sync
```

## En cas de gros pépin

- Le service reste en échec après 10 démarrages en 30 minutes. Pour le relancer : `systemctl reset-failed lg-hue-sync && systemctl start lg-hue-sync`.
- Pour couper le démarrage automatique : `touch $D/DISABLE`, puis redémarrer la TV. `/run` repart vide.
- Le mode secours de Homebrew Channel saute tous les scripts de démarrage, dont celui-ci.

## Ce que ça protège

- **Fichiers de root** : le démon ne peut plus réécrire l'unité, les scripts de démarrage ni les scripts lancés par root (couche 20).
- **Élévation de privilèges** : aucune capacité du noyau, aucun gain par un binaire setuid (couche 30).
- **Noyau** : pas de module, pas de réglage `/proc/sys`, pas de changement d'horloge ni de nom d'hôte (couche 40).
- **Périphériques** : seulement la capture vidéo (couches 40 et 50).
- **Appels système** : seulement ceux d'un service ordinaire ; pas de mémoire à la fois inscriptible et exécutable (couche 60).
- **Ressources** : un démon en boucle ou qui fuit ne peut pas prendre toute la TV (couche 10). Une boucle de plantages s'arrête après 10 essais.
- **Luna** : le démon ne peut plus se faire passer pour le service de capture lui-même (étape 3).
- **Vidage du journal** : l'unité root qui vide `daemon.log` ne peut écrire que dans `state/`. Un lien symbolique posé par le démon ne la mène pas ailleurs.

## Limites connues

- **Pas de contrôleur pids** sur ce noyau : pas de `TasksMax=`. Une rafale de processus ou de fils n'est pas bornée.
- **Pas de filtrage IP** : `IPAddressAllow=` et `IPAddressDeny=` demandent cgroup v2 et BPF. Le démon peut joindre toute adresse.
  Il a besoin de TCP 443 et UDP 2100 vers le pont Hue, de HTTPS vers discovery.meethue.com, et il écoute sur TCP 8088.
- **Le démon reste root (uid 0)** : `/usr/bin/luna-send` est en `-rwx------ root`, sans setuid. Un autre utilisateur ne peut pas le lancer.
  Sans capacité, root ne passe plus outre les droits des fichiers. Mais il reste propriétaire de la plupart des fichiers de la TV : seule la couche 20 l'empêche d'y écrire.
- **`MemoryHigh=` est sans effet** sur cgroups v1. Seule la limite dure `MemoryMax=` agit.
- **journald ne garde rien** : les messages de systemd sur une unité en échec sont perdus. On se fie à `systemctl show -p Result -p ExecMainStatus` et à `state/daemon.log`.
- **Couches 30, 40 et 60** : une partie dépend de seccomp. Sans `+SECCOMP` dans `systemctl --version`, systemd ignore ces réglages en silence.
