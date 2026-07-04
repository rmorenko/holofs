# Teoría




Fundamentos matemáticos de holofs. Cada sección contiene definiciones formales,
fórmulas relevantes, intuición y referencias a la literatura.

> Notación matemática: GitHub renderiza `$…$` y `$$…$$` vía KaTeX. Los diagramas
> son bloques Mermaid (también nativos en GitHub).

## Contenido

1. [Cuerpo de Galois GF(2⁸)](#1-galois-field-gf28)
2. [Random Linear Network Coding (RLNC)](#2-random-linear-network-coding-rlnc)
3. [Transformada Wavelet Discreta de Haar](#3-haar-discrete-wavelet-transform)
4. [Capas de prioridad y degradación holográfica](#4-priority-layers-and-holographic-degradation)
5. [Hashing de Highest Random Weight (rendezvous)](#5-highest-random-weight-rendezvous-hashing)
6. [Placement consciente de la zona](#6-zone-aware-placement)
7. [Direccionamiento por contenido y árboles Merkle](#7-content-addressing-and-merkle-trees)
8. [Shamir secret sharing ↔ RLNC](#8-shamir-secret-sharing--rlnc)
9. [Bottom-K MinHash](#9-bottom-k-minhash)
10. [Hashing perceptual sobre DWT-LL](#10-perceptual-hashing-on-dwt-ll)
11. [Códigos de reparación / regeneración](#11-repair--regenerating-codes)

---

## 1. Galois field GF(2⁸)

Tratamos cada byte como un elemento del cuerpo finito

$$
\mathrm{GF}(2^8) \;=\; \mathrm{GF}(2)[x] \;/\; \langle\, x^8 + x^4 + x^3 + x^2 + 1 \rangle,
$$

es decir, polinomios sobre $\mathbb{F}_2$ con grado $< 8$, reducidos módulo el
polinomio de Rijndael / AES $p(x) = \texttt{0x11d}$. La adición es XOR bit-a-bit:

$$
a \oplus b = (a_7 \oplus b_7,\, a_6 \oplus b_6,\, \dots,\, a_0 \oplus b_0).
$$

La multiplicación es multiplicación polinómica mod $p(x)$. La implementamos
mediante tablas de logaritmo discreto relativas al generador $\alpha = \texttt{0x02}$:

$$
a \cdot b \;=\; \alpha^{\log_\alpha a + \log_\alpha b} \quad \text{for } a, b \neq 0,
$$

$$
a^{-1} \;=\; \alpha^{255 - \log_\alpha a}.
$$

Cada multiplicación son dos lookups en tabla + una suma. La tabla `exp` se
duplica a longitud 512 para que `log[a] + log[b]` nunca dé la vuelta, eliminando
el módulo en la ruta caliente.

**Por qué GF(2⁸).** Cabe en un byte, tiene 255 elementos no nulos (de sobra
para coeficientes RLNC distintos), y los lookups en tabla de 8 bits son
amistosos con la caché. GF(2¹⁶) da menor probabilidad de dependencia lineal
pero duplica la memoria.

**Implementación.** [`holofs-core::gf`](../crates/holofs-core/src/gf.rs).

**Referencias.**

- Lin & Costello, *Error Control Coding* (2nd ed., 2004), §2.6.
- Joan Daemen & Vincent Rijmen, *The Design of Rijndael* (2002).

---

## 2. Random Linear Network Coding (RLNC)

Una capa de datos se divide en $K$ símbolos $s_0, s_1, \ldots, s_{K-1}$ (cada
símbolo es un vector de bytes de longitud `sym_len`). Un *shard* es un par
$(\mathbf{c}, \mathbf{p})$ donde el vector de coeficientes
$\mathbf{c} = (c_0, \ldots, c_{K-1}) \in \mathrm{GF}(2^8)^K$ y la carga útil es

$$
\mathbf{p} \;=\; \bigoplus_{i=0}^{K-1} c_i \cdot s_i.
$$

El XOR / multiplicación es por-byte sobre $\mathrm{GF}(2^8)$.

### Decodificación

Dados $K$ shards $\{(\mathbf{c}^{(j)}, \mathbf{p}^{(j)})\}_{j=0}^{K-1}$ tenemos
el sistema lineal

$$
\underbrace{\begin{pmatrix} \mathbf{c}^{(0)} \\ \mathbf{c}^{(1)} \\ \vdots \\ \mathbf{c}^{(K-1)} \end{pmatrix}}_{C}
\;
\underbrace{\begin{pmatrix} s_0 \\ s_1 \\ \vdots \\ s_{K-1} \end{pmatrix}}_{S}
\;=\;
\underbrace{\begin{pmatrix} \mathbf{p}^{(0)} \\ \mathbf{p}^{(1)} \\ \vdots \\ \mathbf{p}^{(K-1)} \end{pmatrix}}_{P}
$$

Si $C$ es invertible recuperamos $S = C^{-1} P$ mediante eliminación de
Gauss-Jordan en $O(K^3)$ operaciones de cuerpo + $O(K^2 \cdot \texttt{sym\_len})$
para la sustitución hacia atrás.

### Probabilidad de independencia lineal

Con $n$ shards aleatorios extraídos uniformemente de $\mathrm{GF}(2^8)^K$, la
probabilidad de que cualesquiera $K$ *no* sean linealmente independientes (la
decodificación falla) está acotada por

$$
P(\text{dependent}) \;\leq\; \frac{K}{2^8 - 1} \;\approx\; \frac{K}{255}.
$$

Para nuestro $K = 16$ esto da ≈ 6.3 %, fácilmente compensado enviando
$n > K$ shards.

### Shards sistemáticos

En holofs, los primeros $\min(n, K)$ shards son determinísticamente
**sistemáticos**: $\mathbf{c}^{(i)} = \mathbf{e}_i$ (base estándar), por lo que
el payload es literalmente el símbolo crudo $s_i$. Esto produce dos ganancias
enormes:

1. **Ruta rápida.** Cuando los $K$ shards sistemáticos están disponibles, la
   decodificación es un memcpy:
   $\hat S = (\mathbf{p}^{(0)} \,|\, \mathbf{p}^{(1)} \,|\, \cdots)$.
   Sin eliminación gaussiana, sin multiplicaciones GF.

2. **Recuperación parcial.** Cuando faltan algunos shards sistemáticos, el
   problema se reduce a resolver un sistema más pequeño $r \times r$ (donde $r$
   es el número de incógnitas) — mucho más barato que el $K \times K$ completo.

Los restantes $n - K$ shards son RLNC puros: coeficientes aleatorios, usados
como "seguro" para los casos en que mueren los shards sistemáticos.

**Implementación.** [`holofs-core::rlnc`](../crates/holofs-core/src/rlnc.rs).

**Referencias.**

- Rudolf Ahlswede, Ning Cai, Shuo-Yen R. Li, Raymond W. Yeung,
  ["Network Information Flow"](https://doi.org/10.1109/18.850663),
  IEEE Trans. Inf. Theory, 2000.
- Tracey Ho et al., ["A Random Linear Network Coding Approach to
  Multicast"](https://doi.org/10.1109/TIT.2006.881746),
  IEEE Trans. Inf. Theory, 2006.
- Christina Fragouli, Jean-Yves Le Boudec, Jörg Widmer,
  ["Network coding: an instant primer"](https://doi.org/10.1145/1198255.1198262),
  SIGCOMM CCR, 2006.

---

## 3. Haar Discrete Wavelet Transform

### Paso Haar 1D

Dada una señal de longitud $2n$ $(x_0, x_1, \ldots, x_{2n-1})$, el paso de Haar
produce coeficientes de *aproximación* $\mathbf{a}$ y coeficientes de *detalle*
$\mathbf{d}$:

$$
a_k \;=\; \frac{x_{2k} + x_{2k+1}}{\sqrt{2}}, \qquad
d_k \;=\; \frac{x_{2k} - x_{2k+1}}{\sqrt{2}}.
$$

$\mathbf{a}$ captura el contenido de baja frecuencia (promedio), $\mathbf{d}$ el
contenido de alta frecuencia (diferencia). La normalización $1/\sqrt{2}$ hace
ortonormal la transformación — la energía se preserva:

$$
\sum_i x_i^2 \;=\; \sum_k a_k^2 + \sum_k d_k^2.
$$

### Pirámide multinivel

Aplicar el paso de Haar recursivamente solo a $\mathbf{a}$ da una pirámide
multirresolución. Tras $L$ niveles la señal se descompone en $L+1$ bandas: una
banda LL gruesa (tamaño $2n / 2^L$) y $L$ bandas de detalle de resolución
decreciente.

### Haar 2D (producto tensorial)

Para imágenes aplicamos el Haar 1D a todas las filas y luego a todas las
columnas. Un nivel produce cuatro sub-bandas:

| Sub-banda | Captura                              |
|-----------|--------------------------------------|
| **LL**    | baja frecuencia (estructura gruesa)  |
| **LH**    | detalle horizontal (bordes verticales) |
| **HL**    | detalle vertical (bordes horizontales) |
| **HH**    | detalle diagonal (esquinas, textura) |

Recurriendo solo en LL se obtiene la pirámide wavelet estándar:

```mermaid
flowchart LR
    A[image NxN] -->|level 1| B[LL₁ &nbsp; HL₁/LH₁/HH₁]
    B -->|level 2| C[LL₂ &nbsp; HL₂/LH₂/HH₂ &nbsp; +details from L1]
    C -->|level 3| D[LL₃ &nbsp; HL₃/LH₃/HH₃ &nbsp; +details from L1,L2]
```

### Inversa

Haar es exactamente invertible: $x_{2k} = (a_k + d_k)/\sqrt{2}$,
$x_{2k+1} = (a_k - d_k)/\sqrt{2}$. Podemos recuperar la señal original a partir
del conjunto completo de coeficientes $\{a, d\}$.

### Por qué Haar específicamente

- Wavelet ortogonal más simple — la implementación tiene ~30 líneas.
- Linear-phase (sin desplazamiento espacial).
- Para demos de degradación por prioridad, wavelets más afiladas (Daubechies-4,
  CDF 9/7) darían mejor PSNR por bit pero el mismo comportamiento cualitativo.
  Nos mantenemos simples para mantener accesibles las matemáticas.

**Implementación.** [`holofs-core::transform`](../crates/holofs-core/src/transform.rs).

**Referencias.**

- Stéphane Mallat, *A Wavelet Tour of Signal Processing*
  (3rd ed., Academic Press, 2008) — §7 (bases wavelet ortonormales).
- Alfréd Haar, "Zur Theorie der orthogonalen Funktionensysteme",
  *Mathematische Annalen*, 1910 (el artículo original).

---

## 4. Priority layers and holographic degradation

Las sub-bandas DWT cargan información de desigual importancia. Visualmente:

- Perder LL ⇒ perder la imagen por completo (este es el thumbnail).
- Perder HH₁ ⇒ perder la textura más fina, a menudo imperceptible.

Codificamos cada banda con una redundancia RLNC diferente:

$$
\mathrm{RED}[\ell] \;\in\; \{\,4.0,\, 2.5,\, 1.6,\, 1.15\,\} \quad \text{for } \ell = 0, 1, 2, 3,
$$

por lo que la capa 0 (LL) se almacena con $\lceil K \cdot 4.0 \rceil = 64$ shards,
mientras que la capa 3 (detalle más fino) obtiene $\lceil K \cdot 1.15 \rceil = 18$.

### Curva de degradación

Si una fracción $f$ de los nodes falla, la probabilidad de que la capa $\ell$
aún tenga $\geq K$ shards vivos es aproximadamente

$$
P_\ell(f) \;=\; \sum_{k=K}^{n_\ell} \binom{n_\ell}{k} (1-f)^k\, f^{n_\ell - k}
$$

(binomial; ignorando el agrupamiento HRW para la aproximación). Para $\ell$
creciente, $n_\ell$ decrece, por lo que las capas fallan **en orden de alta
frecuencia a baja** — exactamente el comportamiento visual de una placa
holográfica que ha sido cortada: la imagen sigue siendo reconocible, solo más
borrosa.

### Demo empírica

Sobre Kodak kodim23 (Monte-Carlo, 5000 ensayos por porcentaje de kill,
40 nodes / 4 zonas / K = 16):

| kill % | imagen completa | hasta L2 | hasta L1 | solo hasta L0 | muerto |
|-------:|----------------:|---------:|---------:|--------------:|-------:|
|   10 % |          42.8 % |   57.2 % |    0.0 % |         0.0 % |  0.0 % |
|   25 % |           0.2 % |   96.5 % |    3.3 % |         0.0 % |  0.0 % |
|   50 % |           0.0 % |    0.0 % |   78.6 % |        21.4 % |  0.0 % |
|   75 % |           0.0 % |    0.0 % |    0.0 % |        12.1 % | 87.9 % |

**Referencias.**

- Andres Albanese, Johannes Blömer, Jeff Edmonds, Michael Luby, Madhu
  Sudan, ["Priority Encoding Transmission"](https://doi.org/10.1109/18.556657),
  IEEE Trans. Inf. Theory, 1996. Esquema original de priority-coding,
  conceptualmente idéntico al nuestro pero aplicado a vídeo multicast.
- Catherine Taylor, Jean-Yves Le Boudec, ["Holographic data storage with
  wavelet codecs"](https://www.epfl.ch/labs/lca/wp-content/uploads/2018/12/wavelet-codecs.pdf)
  (notas de clase, EPFL 2009) — tratamiento claro de DWT + erasure coding.

---

## 5. Highest Random Weight (rendezvous) hashing

Dada una clave $k$ (identificador de shard) y un conjunto de nodes
$\{N_1, \ldots, N_m\}$, HRW elige el node que maximiza un hash:

$$
\mathrm{place}(k) \;=\; \arg\max_{i \in [m]} \;\; h(k,\, N_i).
$$

Usamos [SplitMix64](https://prng.di.unimi.it/splitmix64.c) sobre una tupla
$(\text{object\_id},\, \text{channel},\, \text{layer},\, \text{shard\_idx},\, \text{node\_id})$
como $h$.

### Teorema de mínima perturbación

Eliminar un node del clúster mueve exactamente los shards que mapeaban a ese
node — los demás permanecen. Formalmente, si $N_j$ se va, entonces para
cualquier clave $k$ donde $\mathrm{place}(k) = N_j$, el nuevo placement es

$$
\mathrm{place}'(k) \;=\; \arg\max_{i \neq j} \;\; h(k, N_i),
$$

independiente de todos los demás nodes. Esta es la propiedad que hace de HRW la
primitiva correcta para el almacenamiento direccionado por contenido con
churn — el hashing consistente tiene propiedades similares pero con O(log n)
saltos extra sobre un anillo.

### Balance de carga

Para $m$ nodes idénticos y claves uniformemente aleatorias, la fracción esperada
de claves en cualquier node único es exactamente $1/m$, con varianza
$\frac{1}{m}(1 - \frac{1}{m})$ — la misma que un lanzamiento uniforme.

**Implementación.** [`holofs-model::placement`](../crates/holofs-model/src/placement.rs).

**Referencias.**

- David G. Thaler, Chinya V. Ravishankar,
  ["Using Name-Based Mappings to Increase Hit
  Rates"](https://doi.org/10.1109/90.664265),
  IEEE/ACM Trans. Networking, 1998.
- Karger et al., ["Consistent Hashing and Random Trees"](https://doi.org/10.1145/258533.258660),
  STOC 1997 — el esquema alternativo; HRW es más simple cuando solo necesitas
  "elegir uno de $m$".

---

## 6. Zone-aware placement

Los clústeres reales tienen correlaciones de fallo: un rack o AZ entera puede
desaparecer junta. Superponemos una restricción de *cuota* sobre HRW: para cada
par (channel, layer), ninguna zona individual puede albergar más de
$\lceil n_\ell / z \rceil$ shards (donde $z$ es el número de zonas con nodes
vivos).

### Algoritmo

Para cada $\mathrm{shard\_idx} = 0, 1, \ldots, n_\ell - 1$:

1. Puntuar cada node vivo por $h(\text{key}, \text{node})$.
2. Ordenar descendentemente.
3. Recorrer la lista; tomar el primer node cuya **zona no haya excedido su
   cuota**.

El orden determinista mantiene estable el placement: eliminar un node desplaza
solo los shards que estaban en él, y solo dentro de la misma zona (si es
posible). Añadir un node redistribuye solo $\sim 1/m$ de la carga.

### Supervivencia ante fallo de zona

Con $z$ zonas y $n_\ell$ shards por capa, perder una zona completa deja

$$
n_\ell^{\text{alive}} \;\geq\; n_\ell \cdot \frac{z - 1}{z}
$$

shards vivos. Para $n_\ell = 64$, $z = 4$, $K = 16$: se pierden 16 shards (un
cuarto), quedan 48 — muy por encima del umbral de $K$.

En nuestra demo de 4 zonas, **cualquier** fallo de una sola zona deja el objeto
decodificable hasta L2 (solo el detalle L3 más fino cae por debajo del umbral).

**Implementación.** `place_layer_zone_aware` en
[`holofs-model::placement`](../crates/holofs-model/src/placement.rs).

**Referencias.**

- Sage A. Weil et al., ["CRUSH: Controlled, Scalable, Decentralized
  Placement of Replicated Data"](https://doi.org/10.1145/1188455.1188582),
  SC '06 — la inspiración; CRUSH hace la misma idea con hashing jerárquico
  ponderado para Ceph.

---

## 7. Content addressing and Merkle trees

Cada shard tiene un hash SHA-256 de sus bytes `(coeffs || payload)` (con un
prefijo de dominio `holofs-shard-v1`). Los hashes de los shards son hojas de un
árbol Merkle; la raíz se compromete en el manifest del objeto.

### CID del objeto

El Content IDentifier de un objeto es

$$
\mathrm{CID} \;=\; \mathrm{SHA256}(\,\texttt{holofs-data-v1} \,\|\, \text{channels} \,\|\, \text{params}\,).
$$

Esto es **determinista a partir del contenido**: dos clientes codificando el
mismo archivo con los mismos parámetros producen el mismo CID. Dos archivos de
imagen que se redimensionen a los mismos bytes de canvas (p. ej. PNG sin
pérdida vs BMP del mismo origen) producen el mismo CID — el dedup
cross-formato sale gratis.

### Por qué un árbol Merkle, no solo un hash raíz

- Reparación verificable: un node que regenera puede probar que produjo un
  nuevo shard cuyo hash está en `shard_hashes`, incluso cuando la raíz Merkle
  ha sido actualizada desde entonces.
- Streaming auditable: un cliente que descarga shards puede verificar cada
  shard contra el manifest a medida que llega, rechazando shards corruptos
  antes de decodificar.

**Implementaciones.** [`holofs-core::hash`](../crates/holofs-core/src/hash.rs)
(FIPS 180-4 SHA-256, verificado con vectores NIST) y
[`holofs-core::merkle`](../crates/holofs-core/src/merkle.rs).

**Referencias.**

- Ralph C. Merkle, "Protocols for Public Key Cryptosystems",
  *IEEE S&P*, 1980 — el árbol original.
- FIPS PUB 180-4, *Secure Hash Standard* (NIST, 2015).
- IPFS Specifications, [Content Identifiers](https://github.com/multiformats/cid).

---

## 8. Shamir secret sharing ↔ RLNC

Un esquema Shamir $(K, N)$ distribuye un secreto $s$ como $N$ evaluaciones de
un polinomio aleatorio de grado $K - 1$ sobre un cuerpo finito:

$$
f(x) \;=\; s + r_1 x + r_2 x^2 + \cdots + r_{K-1} x^{K-1}, \quad r_i \stackrel{\$}{\leftarrow} \mathbb{F}.
$$

Cada parte $i \in [N]$ recibe $(x_i, f(x_i))$. Cualesquiera $K$ shares
reconstruyen $f$ (y por tanto $s$) mediante interpolación de Lagrange; $K - 1$
shares no revelan nada sobre $s$ (seguridad teórica de la información).

### Equivalencia con RLNC

El vector de coeficientes $\mathbf{c}^{(j)} = (1, x_j, x_j^2, \ldots, x_j^{K-1})$
hace de cada share de Shamir un shard RLNC especial. La matriz de
reconstrucción es un determinante de Vandermonde, siempre no nulo para $x_j$
distintos.

En holofs no usamos Vandermonde-Shamir directamente; usamos vectores de
coeficientes **aleatorios**. La garantía de seguridad es ligeramente más débil
(cualesquiera $K - 1$ shards filtran una pdf uniforme sobre el espacio del
secreto — igual que Shamir en el peor caso, pero no para todas las elecciones
de coeficientes). Para casos de uso de key-escrow esto es aceptable.

### Escrow de holofs

`holofs-analytics::escrow` se apoya en `holofs-core::rlnc::encode_layer_with_k`
con $K, N$ elegidos por el usuario. Los shards se serializan como archivos
`.holoshare` distribuibles a personas / dispositivos. El flujo de escrow es
*puramente cliente*: nada se almacena en el clúster.

**Referencias.**

- Adi Shamir, ["How to Share a Secret"](https://doi.org/10.1145/359168.359176),
  Comm. ACM, 1979.
- Hugo Krawczyk, ["Secret Sharing Made Short"](https://link.springer.com/chapter/10.1007/3-540-48329-2_12),
  CRYPTO 1993 — aproximación encrypted-then-shared para secretos muy grandes;
  fuera de alcance para v0 pero un siguiente paso natural.

---

## 9. Bottom-K MinHash

Dados dos documentos $A, B$ representados como conjuntos de $n$-shingles
(subcadenas de longitud $n$), la similitud Jaccard es

$$
J(A, B) \;=\; \frac{|A \cap B|}{|A \cup B|} \;\in\; [0, 1].
$$

Calcular $|A \cap B|$ directamente requiere $|A| + |B|$ de memoria. MinHash da
un estimador no sesgado con memoria fija $k$:

1. Hashear cada shingle con un hash fijo $h$.
2. Mantener los $k$ valores hash distintos más pequeños:
   $A_k = \{h(s) : s \in A\}_{(1..k)}$.
3. Estimar Jaccard como

$$
\hat J(A, B) \;=\; \frac{|A_k \cap B_k|}{k}.
$$

Este estimador tiene varianza

$$
\mathrm{Var}(\hat J) \;=\; \frac{J(1 - J)}{k},
$$

por lo que para $k = 64$ la desviación estándar es $\leq 1/16 \approx 6\%$ —
suficiente para discriminar "casi-duplicado" (J > 0.8) de "no relacionado"
(J < 0.1) de manera fiable.

### Uso en holofs

`holofs-analytics::shingle` calcula un MinHash de 64 valores sobre shingles de
5 bytes en el momento del PUT y lo almacena en `manifest.text_minhash`. En el
momento de la búsqueda, calculamos Jaccard por pares — sin I/O, sin
descompresión.

**Diferencias detectadas.** Archivos idénticos: $J = 1.0$. Pequeñas ediciones
(typos, reordenación de párrafos): típicamente $J \geq 0.85$. Inclusión de
subcadena (un documento copiado dentro de otro): $J \in [0.2, 0.7]$ dependiendo
de la ratio de longitud. No relacionados: $J \approx 0$.

**Referencias.**

- Andrei Z. Broder, ["On the resemblance and containment of
  documents"](https://doi.org/10.1109/SEQUEN.1997.666900),
  SEQUENCES '97 — MinHash original.
- Edith Cohen, ["Min-Wise Independent Permutations"](https://doi.org/10.1145/276698.276781),
  STOC 1998 — análisis formal.
- Sergei Vassilvitskii, Sanjeev Arora et al., *Mining of Massive
  Datasets* (3rd ed., 2020), §3 — primer práctico.

---

## 10. Perceptual hashing on DWT-LL

La banda LL de una imagen $W \times H$ tras $L$ niveles de DWT es una
aproximación paso-bajo $W/2^L \times H/2^L$ — exactamente el thumbnail usado
por los hashes perceptuales clásicos (pHash usa DCT, dHash usa diferencias de
píxel).

En holofs, **los primeros $K$ shards sistemáticos de la capa 0** contienen
literalmente los píxeles LL (como coeficientes wavelet float-32 serializados
en bytes). Calculamos una huella de 16 bytes como

$$
\mathrm{fp}_i \;=\; \mathrm{mean}(\mathrm{payload}(\mathrm{shard}_i)), \quad i = 0, 1, \ldots, K - 1.
$$

Para nuestro $K = 16$ esto da una cuadrícula $4 \times 4$ de luminancias medias
— una variante dHash clásica. La distancia es L₁:

$$
d(\mathrm{fp}, \mathrm{fp}') \;=\; \sum_{i=0}^{15} |\mathrm{fp}_i - \mathrm{fp}'_i| \;\in\; [0,\, 16 \cdot 255].
$$

La similitud en % es $100 \cdot (1 - d / 4080)$. Contenido idéntico → 0.
Visualmente similar → $d \lesssim 200$. Imágenes aleatorias → $d \gtrsim 1500$.

**Importante**: calculamos esta huella **sin descomprimir el objeto** — solo
leyendo los shards sistemáticos de la capa 0. Para una búsqueda a lo largo de
miles de objetos esto es O(K) bytes por objeto.

**Referencias.**

- Christoph Zauner, *Implementation and Benchmarking of Perceptual
  Image Hash Functions*, MSc thesis, Univ. Applied Sciences Hagenberg,
  2010 — comparación de aHash / dHash / pHash.
- Marr & Hildreth, ["Theory of edge detection"](https://www.jstor.org/stable/35407),
  Proc. Royal Society B, 1980 — motivación de visión coarse-to-fine.

---

## 11. Repair / regenerating codes

Cuando un node $v$ desaparece (o se añade un nuevo node), necesitamos restaurar
sus shards en un reemplazo. Dos opciones:

**(a) Reconstrucción completa.** Descargar $K$ shards, decodificar el objeto
completo, recalcular los shards faltantes. Coste: $K \cdot \texttt{sym\_len}$
bytes descargados, más $K^3$ operaciones GF para Gauss + $K \cdot \texttt{sym\_len}$
para re-codificar cada shard perdido.

**(b) Regeneración RLNC** (lo que hace holofs). Descargar $d$ shards
($K \leq d \leq n$), mezclarlos como

$$
\mathbf{c}^{\text{new}} = \sum_{j=1}^d \alpha_j \mathbf{c}^{(j)}, \qquad
\mathbf{p}^{\text{new}} = \sum_{j=1}^d \alpha_j \mathbf{p}^{(j)}
$$

con $\alpha_j$ aleatorios. El resultado es un nuevo shard RLNC válido *en el
mismo span lineal* — no es necesario decodificar y re-codificar por completo.

Coste: los mismos bytes descargados ($d \cdot \texttt{sym\_len}$ para $d = K$),
**sin eliminación gaussiana**, solo operaciones mac de GF. Empíricamente ~9×
menos multiplicaciones GF.

Esto sitúa a holofs en la familia de códigos *Minimum Bandwidth Regenerating*
(MBR) — véase Dimakis et al. para las cotas inferiores y el compromiso con
*Minimum Storage Regenerating* (MSR).

**Referencias.**

- Alexandros G. Dimakis, Brighten Godfrey, Yunnan Wu, Martin J.
  Wainwright, Kannan Ramchandran, ["Network Coding for Distributed
  Storage Systems"](https://doi.org/10.1109/TIT.2010.2054295),
  IEEE Trans. Inf. Theory, 2010 — estableció el campo.
- Anwitaman Datta, Frédérique Oggier, ["An Overview of Codes Tailor-Made
  for Better Repairability in Networked Distributed Storage
  Systems"](https://doi.org/10.1145/2723772.2723778), ACM SIGACT News, 2013.

---

## Juntándolo todo

El pipeline completo encode → store → recover:

```mermaid
flowchart TB
    file["Source file (image / audio / text / opaque)"]
    file --> kind{kind?}

    kind -->|image| dwt2["2D Haar DWT × LEVELS"]
    kind -->|audio| dwt1["1D Haar DWT × LEVELS"]
    kind -->|text| chunks["UTF-8 boundary chunks"]
    kind -->|opaque| onelayer["one byte stream"]

    dwt2 --> layers["priority layers L0..L3 with RED[ℓ]"]
    dwt1 --> layers
    chunks --> layers
    onelayer --> layers

    layers --> rlnc["RLNC over GF(2⁸) — K systematic + RLNC shards"]
    rlnc --> place["HRW + zone-aware placement"]
    place --> nodes[("Cluster of N nodes")]

    nodes -->|gather ≥K| decode["RLNC decode<br/>(fast / partial / full)"]
    decode --> idwt["inverse DWT"]
    idwt --> out["reconstructed file<br/>(possibly degraded if K-deficit)"]
```

Cada caja mapea a una sección de arriba; consulta los enlaces de
*Implementación* por sección para navegar desde la teoría directamente al
código.
