# HeapLens — Modélisation UML complète

**Mémoire de Master en Informatique — Université Adventiste Zurcher**
**Tefison Nathaniah Jean Baptiste — 2026**

Ce document regroupe les sept diagrammes UML du système HeapLens, destinés au chapitre 3 (Conception et Modélisation). Chaque diagramme est fourni sous forme de code Mermaid réutilisable. Pour l'intégration dans le mémoire, numéroter chaque diagramme en figure du chapitre 3 (Figure 3.x), avec légende en dessous et la mention « Source : Auteur ».

> Note : les blocs de code ci-dessous utilisent la syntaxe Mermaid. Ils se rendent automatiquement sur GitHub, GitLab, Obsidian, VS Code (extension Mermaid) et la plupart des éditeurs Markdown. Le septième diagramme (déploiement) est fourni à la fois en Mermaid et en SVG.

---

## Table des diagrammes

| N° | Diagramme | Vue UML | Rôle dans la modélisation |
|----|-----------|---------|---------------------------|
| 1 | Cas d'utilisation | Fonctionnelle | Acteurs et fonctionnalités du système |
| 2 | Classes | Structurelle | Modèle GrapheTas et structures des quatre couches |
| 3 | Séquence | Comportementale | Flux d'un événement d'allocation et non-blocage |
| 4 | États | Comportementale | Cycle de vie d'un nœud d'allocation |
| 5 | Activité | Comportementale | Traitement d'un événement et détection d'anomalies |
| 6 | Composants | Architecturale | Composants déployables et interfaces |
| 7 | Déploiement | Architecturale | Processus déployés sur la machine Windows |

---

## Figure 3.1 — Diagramme de cas d'utilisation

Acteur principal : le développeur. Le programme observé est un acteur secondaire qui émet les événements déclenchant la détection d'anomalie. Les relations « include » indiquent que l'inspection d'un nœud et la signalisation d'anomalie s'inscrivent dans la visualisation temps réel.

```mermaid
graph LR
  Dev["Développeur"]
  Prog["Programme observé"]
  subgraph HeapLens["Système HeapLens"]
    UC1(["Instrumenter un programme"])
    UC2(["Lancer le démon d'observation"])
    UC3(["Visualiser le graphe en temps réel"])
    UC4(["Inspecter un nœud"])
    UC5(["Détecter et signaler une anomalie"])
    UC6(["Filtrer ou mettre en pause la vue"])
    UC7(["Exporter un instantané"])
  end
  Dev --> UC1
  Dev --> UC2
  Dev --> UC3
  Dev --> UC4
  Dev --> UC6
  Dev --> UC7
  Prog --> UC5
  UC4 -.->|"include"| UC3
  UC5 -.->|"include"| UC3
```

---

## Figure 3.2 — Diagramme de classes

`HeapLensAlloc` réalise l'interface `GlobalAlloc` et compose le `RingBuffer`. Le `Daemon` agrège les cinq sous-systèmes (graphe, résolveur, détecteur, persistance, serveur). `OwnershipGraph` compose les `Node`, traduisant directement le modèle formel GrapheTas = (N, A, φ) : les nœuds, leurs arêtes (`edgesOut`) et la méthode `isOrphan(tau)` qui implémente la définition formelle de l'orphelin.

```mermaid
classDiagram
  class GlobalAlloc {
    <<interface>>
    +alloc(Layout) ptr
    +dealloc(ptr, Layout)
    +realloc(ptr, Layout, newSize) ptr
  }
  class HeapLensAlloc {
    -RingBuffer buffer
    -bool recursionGuard
    +alloc(Layout) ptr
    +dealloc(ptr, Layout)
    -record(ptr, Layout, EventKind)
  }
  class AllocEvent {
    +EventKind kind
    +u64 ptr
    +u64 oldPtr
    +u64 size
    +u32 align
    +u64[8] stack
    +u8 stackLen
    +u64 tsNanos
  }
  class EventKind {
    <<enumeration>>
    Alloc
    Dealloc
    Realloc
  }
  class RingBuffer {
    -AtomicUsize head
    -AtomicUsize tail
    -usize capacity
    +push(AllocEvent) bool
    +pop() AllocEvent
  }
  class Daemon {
    -OwnershipGraph graph
    -SymbolResolver resolver
    -AnomalyDetector detector
    -TimeSeriesStore store
    -WebSocketServer server
    +run()
    +onEvent(AllocEvent)
  }
  class OwnershipGraph {
    -Map nodes
    +addNode(AllocEvent)
    +removeNode(u64)
    +inferOwnership(AllocEvent) Edge
    +computeDiff() GraphDiff
    +connectedComponents() int
  }
  class Node {
    +u64 id
    +u64 ptr
    +u64 size
    +String symbol
    +u64 ts
    +bool live
    +u64[] edgesOut
    +isOrphan(tau) bool
  }
  class GraphDiff {
    +Node[] add
    +Node[] update
    +u64[] remove
  }
  class SymbolResolver {
    +resolve(u64[] addrs) String[]
  }
  class AnomalyDetector {
    +detectOrphans(OwnershipGraph) Node[]
    +detectGrowth(OwnershipGraph) Node[]
    +detectStorm(OwnershipGraph) Node[]
  }
  class TimeSeriesStore {
    +persist(AllocEvent)
    +query(window) Stats
  }
  class WebSocketServer {
    +broadcast(GraphDiff)
  }
  class GraphState {
    -Map nodes
    +applyDiff(GraphDiff)
  }
  class ForceSimulation {
    +step()
    -repulsion()
    -attraction()
    -gravity()
  }

  GlobalAlloc <|.. HeapLensAlloc : realise
  HeapLensAlloc *-- RingBuffer
  HeapLensAlloc ..> AllocEvent : cree
  AllocEvent *-- EventKind
  RingBuffer o-- AllocEvent
  Daemon *-- OwnershipGraph
  Daemon *-- SymbolResolver
  Daemon *-- AnomalyDetector
  Daemon *-- TimeSeriesStore
  Daemon *-- WebSocketServer
  Daemon ..> AllocEvent : consomme
  OwnershipGraph *-- Node
  OwnershipGraph ..> GraphDiff : produit
  AnomalyDetector ..> Node : signale
  WebSocketServer ..> GraphDiff : envoie
  GraphState *-- Node
  GraphState ..> GraphDiff : applique
  ForceSimulation ..> GraphState : met a jour
```

---

## Figure 3.3 — Diagramme de séquence (flux d'un événement d'allocation)

Le programme reçoit son `ptr` immédiatement après le `push` dans le tampon, avant que toute la chaîne d'observation ne s'exécute. La boucle « hors du chemin critique » montre que la résolution des symboles, la construction du graphe, la diffusion et le rendu se font de manière asynchrone : c'est la garantie de non-blocage.

```mermaid
sequenceDiagram
    participant P as Programme observe
    participant A as HeapLensAlloc
    participant S as Allocateur systeme
    participant R as RingBuffer SPSC
    participant T as Thread ecrivain
    participant D as Demon
    participant G as OwnershipGraph
    participant W as WebSocketServer
    participant F as App Flutter

    P->>A: alloc(layout)
    A->>S: alloc(layout)
    S-->>A: ptr
    A->>A: capture pile + horodatage
    A->>R: push(AllocEvent)
    A-->>P: ptr (retour immediat, sans blocage)

    loop En continu, hors du chemin critique
        T->>R: pop()
        R-->>T: AllocEvent
        T->>D: envoi via named pipe
        D->>D: resolution des symboles
        D->>G: inferOwnership(event)
        G->>G: addNode + arete de propriete
        D->>G: computeDiff()
        G-->>D: GraphDiff
        D->>W: broadcast(GraphDiff)
        W->>F: diff (WebSocket)
        F->>F: applyDiff + simulation de forces
        F->>F: rendu du graphe
    end
```

---

## Figure 3.4 — Diagramme d'états (cycle de vie d'un nœud)

Le cycle de vie complet d'un nœud : naissance à l'allocation (Vivante), passage possible par Chaude (croissance anormale), et chemin vers Orpheline — l'état qui signale une fuite probable lorsque le propriétaire est libéré mais que le nœud persiste au-delà du seuil τ. Un nœud orphelin jamais désalloué reste bloqué dans cet état : c'est la signature visuelle d'une fuite.

```mermaid
stateDiagram-v2
    [*] --> Vivante : allocation (addNode)
    Vivante --> Chaude : croissance rapide detectee
    Chaude --> Vivante : croissance stabilisee
    Vivante --> Orpheline : proprietaire libere et age superieur a tau
    Chaude --> Orpheline : proprietaire libere et age superieur a tau
    Vivante --> Liberee : dealloc
    Chaude --> Liberee : dealloc
    Orpheline --> Liberee : dealloc (fuite resolue)
    Liberee --> [*] : retrait du graphe (fondu)

    note right of Orpheline
      Indicateur de fuite probable
      noeud vivant ayant perdu son proprietaire
    end note
    note right of Chaude
      Membre d'une grappe
      en croissance anormale
    end note
```

---

## Figure 3.5 — Diagramme d'activité (traitement d'un événement et détection d'anomalies)

Le flux montre les trois branches selon le type d'événement (Alloc, Dealloc, Realloc). Point clé en bas de la branche Dealloc : lorsqu'un nœud libéré avait des enfants vivants, ceux-ci deviennent orphelins candidats. La phase de détection enchaîne ensuite les trois heuristiques (orphelin, croissance, tempête) avant de calculer le diff, persister et diffuser.

```mermaid
flowchart TD
    Start(["Réception d'un AllocEvent"]) --> Type{"Type d'événement ?"}
    Type -->|"Alloc"| Resolve["Résoudre les symboles"]
    Resolve --> Infer["Inférer la propriété via la pile d'appel"]
    Infer --> Add["addNode + arête de propriété"]
    Type -->|"Dealloc"| Find["Trouver le nœud par ptr"]
    Find --> MarkFreed["Marquer le nœud comme libéré"]
    MarkFreed --> HasChildren{"Avait des enfants vivants ?"}
    HasChildren -->|"Oui"| Orphan["Marquer les enfants comme orphelins candidats"]
    HasChildren -->|"Non"| Update["Mise à jour du GrapheTas"]
    Orphan --> Update
    Type -->|"Realloc"| UpdateNode["Mettre à jour ptr et taille"]
    UpdateNode --> Update
    Add --> Update
    Update --> Detect["Exécuter les heuristiques de détection"]
    Detect --> CheckO{"Orphelin dont l'âge dépasse tau ?"}
    CheckO -->|"Oui"| FlagO["Signaler un orphelin"]
    CheckO -->|"Non"| CheckG{"Grappe en croissance anormale ?"}
    FlagO --> CheckG
    CheckG -->|"Oui"| FlagG["Signaler une croissance"]
    CheckG -->|"Non"| CheckS{"Tempête d'allocation ?"}
    FlagG --> CheckS
    CheckS -->|"Oui"| FlagS["Signaler une tempête"]
    CheckS -->|"Non"| Diff["computeDiff"]
    FlagS --> Diff
    Diff --> Persist["Persister la série temporelle"]
    Persist --> Broadcast["Diffuser le diff via WebSocket"]
    Broadcast --> End(["Fin du cycle"])
```

---

## Figure 3.6 — Diagramme de composants

Les trois composants vivent dans trois processus distincts. Les interfaces inter-processus sont explicites : `heaplens-alloc` expose ses événements via le Named Pipe vers le démon ; `heaplens-daemon` diffuse les diffs via WebSocket vers Flutter et persiste les séries temporelles dans SQLite via `rusqlite`.

```mermaid
flowchart LR
    subgraph PROC1["Processus observé"]
        subgraph C1["Composant heaplens-alloc"]
            GA["GlobalAlloc impl"]
            RB["RingBuffer SPSC"]
            TW["Thread écrivain"]
            GA --> RB --> TW
        end
    end

    subgraph PROC2["Processus démon"]
        subgraph C2["Composant heaplens-daemon"]
            NPS["Serveur Named Pipe"]
            SR["SymbolResolver"]
            OG["OwnershipGraph"]
            AD["AnomalyDetector"]
            TS["TimeSeriesStore"]
            WS["WebSocketServer"]
            NPS --> SR --> OG
            OG --> AD
            OG --> TS
            OG --> WS
        end
    end

    subgraph PROC3["Processus interface"]
        subgraph C3["Composant heaplens-flutter"]
            WC["Client WebSocket"]
            GS["GraphState"]
            FS["ForceSimulation"]
            GC["GraphCanvas"]
            WC --> GS --> FS --> GC
        end
    end

    DB[("SQLite")]

    TW -->|"Named Pipe : trames binaires"| NPS
    WS -->|"WebSocket : diffs JSON"| WC
    TS -->|"rusqlite"| DB
```

---

## Figure 3.7 — Diagramme de déploiement

La machine Windows héberge les trois processus : le processus observé pousse ses événements vers le démon par named pipe ; le démon persiste dans SQLite via `rusqlite` et diffuse les diffs vers l'interface Flutter par WebSocket en loopback. L'isolation en trois processus distincts garantit que l'observation reste transparente pour le programme surveillé.

### Version Mermaid

```mermaid
flowchart TB
    subgraph Machine["Poste de developpement Windows"]
        subgraph P1["Processus observe"]
            A1["programme Rust + heaplens-alloc"]
        end
        subgraph P2["Processus demon"]
            A2["heaplens-daemon"]
        end
        subgraph P3["Processus interface"]
            A4["heaplens-flutter"]
        end
        A3[("Base SQLite : heaplens.db")]
    end

    A1 -->|"Named Pipe Windows, trames binaires"| A2
    A2 -->|"rusqlite"| A3
    A2 -->|"WebSocket loopback, diffs JSON"| A4
```

### Version SVG (exportable en image)

```svg
<svg width="100%" viewBox="0 0 680 456" role="img" xmlns="http://www.w3.org/2000/svg">
<title>Diagramme de déploiement de HeapLens</title>
<desc>Les trois processus de HeapLens déployés sur une machine Windows : le processus observé communique avec le démon via un named pipe, le démon diffuse vers l'application Flutter via WebSocket et persiste les données dans une base SQLite.</desc>
<defs>
<marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M2 1L8 5L2 9" fill="none" stroke="#888780" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></marker>
</defs>
<rect x="40" y="44" width="600" height="372" rx="16" fill="#F1EFE8" stroke="#5F5E5A" stroke-width="0.5"/>
<text x="60" y="72" font-family="sans-serif" font-size="14" font-weight="500" fill="#2C2C2A">Poste de developpement (Windows)</text>

<rect x="80" y="104" width="300" height="58" rx="8" fill="#E6F1FB" stroke="#185FA5" stroke-width="0.5"/>
<text x="230" y="126" font-family="sans-serif" font-size="14" font-weight="500" fill="#0C447C" text-anchor="middle">Processus observe</text>
<text x="230" y="145" font-family="sans-serif" font-size="12" fill="#185FA5" text-anchor="middle">programme Rust + heaplens-alloc</text>

<line x1="230" y1="162" x2="230" y2="210" stroke="#888780" stroke-width="1.5" marker-end="url(#arrow)"/>
<text x="246" y="181" font-family="sans-serif" font-size="12" fill="#5F5E5A">Named Pipe</text>
<text x="246" y="197" font-family="sans-serif" font-size="12" fill="#5F5E5A">(trames binaires)</text>

<rect x="80" y="214" width="300" height="58" rx="8" fill="#E6F1FB" stroke="#185FA5" stroke-width="0.5"/>
<text x="230" y="236" font-family="sans-serif" font-size="14" font-weight="500" fill="#0C447C" text-anchor="middle">Processus demon</text>
<text x="230" y="255" font-family="sans-serif" font-size="12" fill="#185FA5" text-anchor="middle">heaplens-daemon</text>

<rect x="440" y="215" width="170" height="56" rx="8" fill="#E1F5EE" stroke="#0F6E56" stroke-width="0.5"/>
<text x="525" y="236" font-family="sans-serif" font-size="14" font-weight="500" fill="#085041" text-anchor="middle">Base SQLite</text>
<text x="525" y="255" font-family="sans-serif" font-size="12" fill="#0F6E56" text-anchor="middle">heaplens.db</text>

<line x1="380" y1="243" x2="438" y2="243" stroke="#888780" stroke-width="1.5" marker-end="url(#arrow)"/>
<text x="409" y="236" font-family="sans-serif" font-size="12" fill="#5F5E5A" text-anchor="middle">rusqlite</text>

<line x1="230" y1="272" x2="230" y2="330" stroke="#888780" stroke-width="1.5" marker-end="url(#arrow)"/>
<text x="246" y="296" font-family="sans-serif" font-size="12" fill="#5F5E5A">WebSocket</text>
<text x="246" y="312" font-family="sans-serif" font-size="12" fill="#5F5E5A">(diffs JSON)</text>

<rect x="80" y="330" width="300" height="58" rx="8" fill="#E6F1FB" stroke="#185FA5" stroke-width="0.5"/>
<text x="230" y="352" font-family="sans-serif" font-size="14" font-weight="500" fill="#0C447C" text-anchor="middle">Processus interface</text>
<text x="230" y="371" font-family="sans-serif" font-size="12" fill="#185FA5" text-anchor="middle">heaplens-flutter</text>
</svg>
```

---

## Notes d'intégration au mémoire

- Numéroter les diagrammes en figures du chapitre 3 (Figure 3.1 à 3.7), légende en dessous, en italique, centrée, suivie de « Source : Auteur ».
- Pour obtenir une image à partir du code Mermaid : utiliser [mermaid.live](https://mermaid.live) (coller le code, exporter en PNG ou SVG), ou l'extension Mermaid de VS Code.
- Le diagramme de déploiement est fourni en SVG : il peut être ouvert directement dans un navigateur puis exporté, ou inséré tel quel.
- Chaque diagramme correspond à une vue UML standard, ce qui assure une modélisation complète (fonctionnelle, structurelle, comportementale, architecturale).
