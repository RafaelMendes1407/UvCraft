# UvCraft — Modernização da Interface e Implementação de Ferramentas UV

## Contexto do projeto

O UvCraft é uma aplicação desktop especializada em edição, abertura e organização de UVs de modelos 3D, com funcionalidades semelhantes às encontradas no RizomUV e no Blender.

A aplicação já possui uma base funcional. O objetivo desta tarefa é modernizar sua interface e implementar novas ferramentas de edição UV, preservando a arquitetura, as funcionalidades e o desempenho existentes.

**Antes de qualquer implementação, analise o projeto completo para compreender:**

- Linguagem, framework gráfico e bibliotecas utilizadas.
- Organização dos módulos e responsabilidades.
- Representação interna da malha 3D, vértices, edges, faces, seams e ilhas UV.
- Sistema de seleção e interação com o mouse.
- Algoritmos existentes de unwrap, relaxamento, transformação e packing.
- Sistema de histórico Undo/Redo.
- Estrutura de testes e mecanismos de persistência.

Reutilize os componentes e algoritmos existentes sempre que possível. Não substitua partes funcionais desnecessariamente.

---

## 1. Modernização da interface e identidade visual

Na pasta do projeto existem duas imagens de referência:

1. Um ícone oficial para o UvCraft.
2. Uma imagem contendo o protótipo visual da interface desejada.

Localize essas imagens e utilize-as como referência principal.

### Requisitos

- Utilizar o ícone fornecido como identidade visual oficial da aplicação.
- Aplicar o ícone à janela principal e ao executável, conforme as possibilidades do framework e do sistema operacional.
- Modernizar a interface seguindo o protótipo fornecido.
- Adotar uma identidade visual escura, com tons de grafite, detalhes em laranja e tipografia moderna.
- Implementar painéis redimensionáveis.
- Manter o viewport 3D e o editor UV 2D visíveis simultaneamente.
- Organizar ferramentas em menus, barras de ferramentas e painéis contextuais.
- Fornecer feedback visual claro para seleções, operações e ferramentas ativas.
- Preservar atalhos e comportamentos existentes, salvo quando houver conflito justificado.

A imagem é uma referência de design, não uma especificação literal. Priorize usabilidade, consistência e integração com a aplicação real.

**Importante:** não utilizar a imagem da interface como plano de fundo. Reconstruir a interface com componentes reais, funcionais e interativos.

---

## 2. Movimentação independente de ilhas UV no editor 2D

Implementar a movimentação individual de ilhas UV utilizando diretamente o mouse.

### Comportamento esperado

- Clicar sobre uma ilha UV para selecioná-la.
- Segurar o botão esquerdo e arrastar para movimentar a ilha.
- Atualizar sua posição visual continuamente durante o arraste.
- Ao soltar o botão, confirmar a nova posição.
- Permitir selecionar múltiplas ilhas e movimentá-las conjuntamente.
- Preservar as coordenadas relativas dos vértices dentro de cada ilha.
- Não alterar ilhas que não participam da seleção.
- Permitir movimentar ilhas para fora do espaço UV 0–1, caso o editor suporte esse fluxo.

### Requisitos técnicos

- Implementar hit-testing preciso de ilhas, considerando faces e polígonos UV.
- Converter corretamente coordenadas do mouse para o espaço UV.
- Respeitar zoom, pan e transformações da câmera 2D.
- Evitar saltos de posição no início do arraste.
- Não recalcular unwrap durante uma simples translação.
- Atualizar somente os dados e buffers gráficos necessários.
- Registrar cada operação de arraste como uma única ação no histórico Undo/Redo.

### Critérios de aceitação

Uma ilha pode ser selecionada e arrastada independentemente, sem modificar a geometria interna, a escala ou a posição das demais ilhas.

---

## 3. Seleção de caminhos de edges no viewport 3D

Implementar uma ferramenta de seleção de caminhos de edges semelhante ao recurso de shortest path do Blender.

### Comportamento esperado

1. O usuário seleciona uma edge inicial.
2. Segurando Shift, clica em outra edge.
3. O sistema calcula um caminho de edges conectadas entre as duas seleções.
4. Todas as edges pertencentes ao caminho são selecionadas.
5. O usuário pode marcar ou desmarcar o caminho inteiro como seam.

### Requisitos funcionais

- Suportar seleção de edges individuais.
- Suportar seleção de caminhos contínuos.
- Permitir acrescentar novos caminhos à seleção existente.
- Exibir uma prévia visual do caminho quando viável.
- Diferenciar visualmente edges selecionadas e seams já confirmadas.
- Permitir marcar e remover seams em lote.
- Manter sincronização com os dados topológicos da malha.

### Algoritmo

Utilizar a conectividade real da malha, e não apenas a proximidade visual entre edges.

Construir ou reutilizar uma representação topológica adequada, como adjacency graph ou half-edge.

Para o cálculo do caminho, utilizar um algoritmo de busca em grafo, como Dijkstra ou A*, com pesos apropriados para a geometria.

O caminho deve ser determinístico e priorizar uma trajetória coerente sobre a superfície.

Quando houver caminhos de custo equivalente, utilizar critérios estáveis de desempate.

Não permitir que o caminho atravesse edges desconectadas ou partes independentes da malha.

### Interação

- Clique: selecionar edge.
- Shift + clique: calcular caminho até a edge selecionada.
- Comando Mark Seam: marcar todas as edges selecionadas.
- Comando Clear Seam: remover a marcação das edges selecionadas.

Integrar os comandos ao sistema de atalhos existente, evitando conflitos.

### Critérios de aceitação

Ao selecionar duas edges conectadas, o sistema identifica e destaca corretamente o caminho entre elas, permitindo marcá-lo como seam em uma única operação.

---

## 4. Agrupamento, espelhamento e sobreposição de ilhas UV

Implementar ferramentas para organizar ilhas UV que compartilham geometrias equivalentes, especialmente ilhas espelhadas.

O objetivo é permitir que duas ou mais ilhas utilizem a mesma região da textura, reduzindo o consumo de espaço UV.

### Funcionalidades

**4.1. Agrupamento de ilhas**

- Permitir selecionar duas ou mais ilhas e agrupá-las.
- Preservar a identidade individual de cada ilha.
- Permitir mover, rotacionar e escalar o grupo.
- Permitir desfazer o agrupamento.
- Manter as transformações relativas entre as ilhas do grupo.

**4.2. Detecção de ilhas equivalentes**

Implementar uma opção para identificar ilhas com formatos e proporções semelhantes.

Considerar:

- Quantidade e conectividade de vértices.
- Comprimentos relativos das edges.
- Proporções geométricas.
- Correspondência topológica.
- Possibilidade de equivalência por reflexão.

Utilizar tolerâncias numéricas configuráveis e evitar considerar ilhas incompatíveis como equivalentes apenas porque seus contornos são parecidos.

**4.3. Espelhamento de ilhas**

Implementar comandos:

- Flip Horizontal.
- Flip Vertical.
- Mirror Island.
- Stack Islands.
- Unstack Islands.

O espelhamento deve operar nas coordenadas UV, preservando a geometria 3D.

**4.4. Sobreposição de ilhas (Stacking)**

Permitir que ilhas equivalentes ocupem exatamente a mesma região UV.

Ao executar Stack Islands:

1. Identificar a ilha de referência.
2. Determinar a correspondência entre os vértices UV.
3. Calcular a transformação necessária para alinhar as demais ilhas.
4. Aplicar translação, rotação, escala uniforme e reflexão, conforme necessário.
5. Preservar as ilhas como entidades independentes.
6. Registrar a relação de stacking para operações futuras.

A sobreposição precisa ser precisa e não apenas aproximada visualmente.

### Integração com Packing

O algoritmo de packing deve reconhecer grupos de ilhas sobrepostas.

- Tratar cada stack como uma única ocupação espacial.
- Não separar automaticamente ilhas agrupadas.
- Preservar sobreposições intencionais.
- Distinguir overlaps intencionais de colisões UV indesejadas.
- Permitir desativar a preservação de stacks por configuração.

### Critérios de aceitação

Duas ilhas geometricamente equivalentes e espelhadas podem ser alinhadas e sobrepostas, compartilhando a mesma área da textura. O packing deve preservar essa relação.

---

## 5. Retificação de ilhas curvas (Straighten UV Islands)

Implementar uma ferramenta capaz de transformar ilhas UV curvas em faixas retas, mantendo uma distribuição coerente da superfície.

### Exemplo

Uma ilha que representa um arco, uma faixa circular ou uma seção curva de um cilindro deve poder ser reorganizada em uma faixa aproximadamente retangular.

### Funcionalidades

Adicionar as operações:

- Straighten Island.
- Straighten Selected Edges.
- Align UV Horizontally.
- Align UV Vertically.

### Comportamento esperado

- Permitir selecionar uma ilha inteira ou uma sequência de edges UV.
- Identificar a direção principal da ilha ou utilizar uma direção definida pelo usuário.
- Reorganizar as coordenadas UV para produzir uma faixa reta.
- Preservar, tanto quanto possível, as proporções entre os segmentos.
- Evitar distorções excessivas e inversões de faces.
- Preservar a conectividade da ilha.
- Permitir aplicar Relax após a retificação.

### Estratégia algorítmica

Para ilhas com estrutura de faixa:

1. Identificar as bordas longitudinais e transversais.
2. Calcular o comprimento acumulado ao longo da faixa.
3. Mapear esse comprimento para um eixo UV.
4. Distribuir a largura da faixa ao longo do eixo perpendicular.
5. Preservar proporções locais de comprimento e largura.
6. Validar a orientação e a integridade das faces UV.

Para geometrias que não possuem uma estrutura de faixa identificável, não aplicar uma transformação destrutiva arbitrária. Informar a limitação ou solicitar uma seleção de referência.

### Critérios de aceitação

Uma ilha UV em formato de arco pode ser transformada em uma faixa reta e organizada, sem perder faces ou produzir distorções desnecessárias.

---

## 6. Recalcular escala proporcional das ilhas (Texel Density)

Implementar uma ferramenta para recalcular o tamanho relativo das ilhas UV, considerando a área real da geometria 3D.

Essa funcionalidade deve ser equivalente, em conceito, ao Average Island Scale do Blender e às ferramentas de normalização de texel density.

### Objetivo

Garantir que ilhas UV selecionadas apresentem densidade de textura consistente.

Uma superfície 3D maior deve ocupar proporcionalmente mais área UV do que uma superfície menor, independentemente do tamanho atual de suas ilhas.

### Funcionalidades

- Average Island Scale.
- Normalize Texel Density.
- Set Texel Density, quando houver resolução de textura definida.
- Preserve Island Position, quando possível.

### Cálculo

Para cada ilha, calcular:

- Área total das faces correspondentes no espaço 3D.
- Área total das faces no espaço UV.
- Fator de escala necessário para alcançar uma densidade consistente.

A escala linear deve respeitar a relação entre as áreas, utilizando a raiz quadrada do fator de área.

O cálculo deve considerar triangulação consistente para polígonos não triangulares.

### Requisitos

- Aplicar escala uniforme por ilha.
- Preservar as proporções internas.
- Preservar a orientação.
- Utilizar o centro da ilha como pivô padrão.
- Operar somente sobre as ilhas selecionadas.
- Não mover ilhas desnecessariamente.
- Permitir packing posteriormente.
- Tratar áreas degeneradas ou próximas de zero de maneira segura.

### Critérios de aceitação

Ao selecionar ilhas com tamanhos UV desproporcionais e executar Average Island Scale, elas passam a apresentar densidade de textura equivalente, respeitando as áreas de suas superfícies 3D.

---

## 7. Integração e consistência das ferramentas

Todas as novas funcionalidades devem compartilhar os sistemas existentes de seleção, transformação, histórico e renderização.

### Requisitos obrigatórios

- Undo/Redo para todas as operações.
- Atualização sincronizada entre viewport 3D e editor UV.
- Feedback visual para seleções e ferramentas ativas.
- Tratamento de erros e estados inválidos.
- Compatibilidade com o sistema de arquivos e formatos 3D já suportados.
- Preservação das coordenadas UV ao salvar e reabrir modelos.
- Operações eficientes em malhas de alta densidade.
- Evitar bloqueios prolongados da interface durante cálculos complexos.
- Preservar dados existentes, incluindo seams, UV sets e relações de ilhas.

### Testes

Criar testes para:

- Arraste individual e múltiplo de ilhas.
- Conversão entre coordenadas de tela e UV.
- Cálculo de caminhos de edges.
- Seleção em malhas desconectadas.
- Marcação e remoção de seams.
- Agrupamento e desagrupamento.
- Detecção e stacking de ilhas espelhadas.
- Preservação de stacks durante packing.
- Retificação de faixas curvas.
- Normalização de escala UV.
- Undo/Redo.
- Salvamento e carregamento.

Utilizar geometrias de teste conhecidas, como cubos, cilindros, arcos, faixas curvas e objetos simétricos.

---

## 8. Estratégia de implementação

Realize as modificações de forma incremental, na seguinte ordem:

**Fase 1 — Análise e arquitetura**

Inspecionar o código existente, identificar os componentes afetados e definir uma estratégia que minimize alterações desnecessárias.

**Fase 2 — Interface**

Aplicar a identidade visual e reorganizar a interface conforme as imagens de referência.

**Fase 3 — Interação**

Implementar movimentação de ilhas UV e seleção de caminhos de edges.

**Fase 4 — Manipulação UV**

Implementar agrupamento, espelhamento, stacking e retificação de ilhas.

**Fase 5 — Normalização**

Implementar Average Island Scale e normalização de texel density.

**Fase 6 — Integração**

Adaptar packing, histórico, persistência e demais sistemas afetados.

**Fase 7 — Validação**

Executar testes automatizados, corrigir regressões e validar os fluxos de trabalho.

### Diretrizes de execução

- Não reescrever a aplicação do zero.
- Não introduzir dependências sem necessidade justificada.
- Respeitar os padrões arquiteturais existentes.
- Separar algoritmos geométricos da camada de interface.
- Evitar duplicação de lógica.
- Preferir estruturas de dados reutilizáveis.
- Não implementar funcionalidades apenas visualmente: todos os controles devem executar operações reais.
- Não substituir implementações corretas por aproximações simplificadas.
- Não declarar uma funcionalidade concluída sem validá-la.
- Caso alguma funcionalidade dependa de uma mudança arquitetural relevante, explicar o motivo antes de realizá-la.

## 9. Entrega esperada

Ao finalizar, apresentar:

1. Resumo das funcionalidades implementadas.
2. Principais arquivos e módulos modificados.
3. Algoritmos e estruturas de dados utilizados.
4. Atalhos e interações disponíveis.
5. Testes executados e seus resultados.
6. Limitações conhecidas ou funcionalidades ainda pendentes.

**Resultado esperado:** transformar o UvCraft em um editor UV profissional, moderno e eficiente, com manipulação direta de ilhas, seleção inteligente de seams, stacking, retificação de UVs e normalização de escala, mantendo a estabilidade e a arquitetura da aplicação existente.# UvCraft — Modernização da Interface e Implementação de Ferramentas UV

## Contexto do projeto

O UvCraft é uma aplicação desktop especializada em edição, abertura e organização de UVs de modelos 3D, com funcionalidades semelhantes às encontradas no RizomUV e no Blender.

A aplicação já possui uma base funcional. O objetivo desta tarefa é modernizar sua interface e implementar novas ferramentas de edição UV, preservando a arquitetura, as funcionalidades e o desempenho existentes.

**Antes de qualquer implementação, analise o projeto completo para compreender:**

- Linguagem, framework gráfico e bibliotecas utilizadas.
- Organização dos módulos e responsabilidades.
- Representação interna da malha 3D, vértices, edges, faces, seams e ilhas UV.
- Sistema de seleção e interação com o mouse.
- Algoritmos existentes de unwrap, relaxamento, transformação e packing.
- Sistema de histórico Undo/Redo.
- Estrutura de testes e mecanismos de persistência.

Reutilize os componentes e algoritmos existentes sempre que possível. Não substitua partes funcionais desnecessariamente.

---

## 1. Modernização da interface e identidade visual

Na pasta do projeto existem duas imagens de referência:

1. Um ícone oficial para o UvCraft.
2. Uma imagem contendo o protótipo visual da interface desejada.

Localize essas imagens e utilize-as como referência principal.

### Requisitos

- Utilizar o ícone fornecido como identidade visual oficial da aplicação.
- Aplicar o ícone à janela principal e ao executável, conforme as possibilidades do framework e do sistema operacional.
- Modernizar a interface seguindo o protótipo fornecido.
- Adotar uma identidade visual escura, com tons de grafite, detalhes em laranja e tipografia moderna.
- Implementar painéis redimensionáveis.
- Manter o viewport 3D e o editor UV 2D visíveis simultaneamente.
- Organizar ferramentas em menus, barras de ferramentas e painéis contextuais.
- Fornecer feedback visual claro para seleções, operações e ferramentas ativas.
- Preservar atalhos e comportamentos existentes, salvo quando houver conflito justificado.

A imagem é uma referência de design, não uma especificação literal. Priorize usabilidade, consistência e integração com a aplicação real.

**Importante:** não utilizar a imagem da interface como plano de fundo. Reconstruir a interface com componentes reais, funcionais e interativos.

---

## 2. Movimentação independente de ilhas UV no editor 2D

Implementar a movimentação individual de ilhas UV utilizando diretamente o mouse.

### Comportamento esperado

- Clicar sobre uma ilha UV para selecioná-la.
- Segurar o botão esquerdo e arrastar para movimentar a ilha.
- Atualizar sua posição visual continuamente durante o arraste.
- Ao soltar o botão, confirmar a nova posição.
- Permitir selecionar múltiplas ilhas e movimentá-las conjuntamente.
- Preservar as coordenadas relativas dos vértices dentro de cada ilha.
- Não alterar ilhas que não participam da seleção.
- Permitir movimentar ilhas para fora do espaço UV 0–1, caso o editor suporte esse fluxo.

### Requisitos técnicos

- Implementar hit-testing preciso de ilhas, considerando faces e polígonos UV.
- Converter corretamente coordenadas do mouse para o espaço UV.
- Respeitar zoom, pan e transformações da câmera 2D.
- Evitar saltos de posição no início do arraste.
- Não recalcular unwrap durante uma simples translação.
- Atualizar somente os dados e buffers gráficos necessários.
- Registrar cada operação de arraste como uma única ação no histórico Undo/Redo.

### Critérios de aceitação

Uma ilha pode ser selecionada e arrastada independentemente, sem modificar a geometria interna, a escala ou a posição das demais ilhas.

---

## 3. Seleção de caminhos de edges no viewport 3D

Implementar uma ferramenta de seleção de caminhos de edges semelhante ao recurso de shortest path do Blender.

### Comportamento esperado

1. O usuário seleciona uma edge inicial.
2. Segurando Shift, clica em outra edge.
3. O sistema calcula um caminho de edges conectadas entre as duas seleções.
4. Todas as edges pertencentes ao caminho são selecionadas.
5. O usuário pode marcar ou desmarcar o caminho inteiro como seam.

### Requisitos funcionais

- Suportar seleção de edges individuais.
- Suportar seleção de caminhos contínuos.
- Permitir acrescentar novos caminhos à seleção existente.
- Exibir uma prévia visual do caminho quando viável.
- Diferenciar visualmente edges selecionadas e seams já confirmadas.
- Permitir marcar e remover seams em lote.
- Manter sincronização com os dados topológicos da malha.

### Algoritmo

Utilizar a conectividade real da malha, e não apenas a proximidade visual entre edges.

Construir ou reutilizar uma representação topológica adequada, como adjacency graph ou half-edge.

Para o cálculo do caminho, utilizar um algoritmo de busca em grafo, como Dijkstra ou A*, com pesos apropriados para a geometria.

O caminho deve ser determinístico e priorizar uma trajetória coerente sobre a superfície.

Quando houver caminhos de custo equivalente, utilizar critérios estáveis de desempate.

Não permitir que o caminho atravesse edges desconectadas ou partes independentes da malha.

### Interação

- Clique: selecionar edge.
- Shift + clique: calcular caminho até a edge selecionada.
- Comando Mark Seam: marcar todas as edges selecionadas.
- Comando Clear Seam: remover a marcação das edges selecionadas.

Integrar os comandos ao sistema de atalhos existente, evitando conflitos.

### Critérios de aceitação

Ao selecionar duas edges conectadas, o sistema identifica e destaca corretamente o caminho entre elas, permitindo marcá-lo como seam em uma única operação.

---

## 4. Agrupamento, espelhamento e sobreposição de ilhas UV

Implementar ferramentas para organizar ilhas UV que compartilham geometrias equivalentes, especialmente ilhas espelhadas.

O objetivo é permitir que duas ou mais ilhas utilizem a mesma região da textura, reduzindo o consumo de espaço UV.

### Funcionalidades

**4.1. Agrupamento de ilhas**

- Permitir selecionar duas ou mais ilhas e agrupá-las.
- Preservar a identidade individual de cada ilha.
- Permitir mover, rotacionar e escalar o grupo.
- Permitir desfazer o agrupamento.
- Manter as transformações relativas entre as ilhas do grupo.

**4.2. Detecção de ilhas equivalentes**

Implementar uma opção para identificar ilhas com formatos e proporções semelhantes.

Considerar:

- Quantidade e conectividade de vértices.
- Comprimentos relativos das edges.
- Proporções geométricas.
- Correspondência topológica.
- Possibilidade de equivalência por reflexão.

Utilizar tolerâncias numéricas configuráveis e evitar considerar ilhas incompatíveis como equivalentes apenas porque seus contornos são parecidos.

**4.3. Espelhamento de ilhas**

Implementar comandos:

- Flip Horizontal.
- Flip Vertical.
- Mirror Island.
- Stack Islands.
- Unstack Islands.

O espelhamento deve operar nas coordenadas UV, preservando a geometria 3D.

**4.4. Sobreposição de ilhas (Stacking)**

Permitir que ilhas equivalentes ocupem exatamente a mesma região UV.

Ao executar Stack Islands:

1. Identificar a ilha de referência.
2. Determinar a correspondência entre os vértices UV.
3. Calcular a transformação necessária para alinhar as demais ilhas.
4. Aplicar translação, rotação, escala uniforme e reflexão, conforme necessário.
5. Preservar as ilhas como entidades independentes.
6. Registrar a relação de stacking para operações futuras.

A sobreposição precisa ser precisa e não apenas aproximada visualmente.

### Integração com Packing

O algoritmo de packing deve reconhecer grupos de ilhas sobrepostas.

- Tratar cada stack como uma única ocupação espacial.
- Não separar automaticamente ilhas agrupadas.
- Preservar sobreposições intencionais.
- Distinguir overlaps intencionais de colisões UV indesejadas.
- Permitir desativar a preservação de stacks por configuração.

### Critérios de aceitação

Duas ilhas geometricamente equivalentes e espelhadas podem ser alinhadas e sobrepostas, compartilhando a mesma área da textura. O packing deve preservar essa relação.

---

## 5. Retificação de ilhas curvas (Straighten UV Islands)

Implementar uma ferramenta capaz de transformar ilhas UV curvas em faixas retas, mantendo uma distribuição coerente da superfície.

### Exemplo

Uma ilha que representa um arco, uma faixa circular ou uma seção curva de um cilindro deve poder ser reorganizada em uma faixa aproximadamente retangular.

### Funcionalidades

Adicionar as operações:

- Straighten Island.
- Straighten Selected Edges.
- Align UV Horizontally.
- Align UV Vertically.

### Comportamento esperado

- Permitir selecionar uma ilha inteira ou uma sequência de edges UV.
- Identificar a direção principal da ilha ou utilizar uma direção definida pelo usuário.
- Reorganizar as coordenadas UV para produzir uma faixa reta.
- Preservar, tanto quanto possível, as proporções entre os segmentos.
- Evitar distorções excessivas e inversões de faces.
- Preservar a conectividade da ilha.
- Permitir aplicar Relax após a retificação.

### Estratégia algorítmica

Para ilhas com estrutura de faixa:

1. Identificar as bordas longitudinais e transversais.
2. Calcular o comprimento acumulado ao longo da faixa.
3. Mapear esse comprimento para um eixo UV.
4. Distribuir a largura da faixa ao longo do eixo perpendicular.
5. Preservar proporções locais de comprimento e largura.
6. Validar a orientação e a integridade das faces UV.

Para geometrias que não possuem uma estrutura de faixa identificável, não aplicar uma transformação destrutiva arbitrária. Informar a limitação ou solicitar uma seleção de referência.

### Critérios de aceitação

Uma ilha UV em formato de arco pode ser transformada em uma faixa reta e organizada, sem perder faces ou produzir distorções desnecessárias.

---

## 6. Recalcular escala proporcional das ilhas (Texel Density)

Implementar uma ferramenta para recalcular o tamanho relativo das ilhas UV, considerando a área real da geometria 3D.

Essa funcionalidade deve ser equivalente, em conceito, ao Average Island Scale do Blender e às ferramentas de normalização de texel density.

### Objetivo

Garantir que ilhas UV selecionadas apresentem densidade de textura consistente.

Uma superfície 3D maior deve ocupar proporcionalmente mais área UV do que uma superfície menor, independentemente do tamanho atual de suas ilhas.

### Funcionalidades

- Average Island Scale.
- Normalize Texel Density.
- Set Texel Density, quando houver resolução de textura definida.
- Preserve Island Position, quando possível.

### Cálculo

Para cada ilha, calcular:

- Área total das faces correspondentes no espaço 3D.
- Área total das faces no espaço UV.
- Fator de escala necessário para alcançar uma densidade consistente.

A escala linear deve respeitar a relação entre as áreas, utilizando a raiz quadrada do fator de área.

O cálculo deve considerar triangulação consistente para polígonos não triangulares.

### Requisitos

- Aplicar escala uniforme por ilha.
- Preservar as proporções internas.
- Preservar a orientação.
- Utilizar o centro da ilha como pivô padrão.
- Operar somente sobre as ilhas selecionadas.
- Não mover ilhas desnecessariamente.
- Permitir packing posteriormente.
- Tratar áreas degeneradas ou próximas de zero de maneira segura.

### Critérios de aceitação

Ao selecionar ilhas com tamanhos UV desproporcionais e executar Average Island Scale, elas passam a apresentar densidade de textura equivalente, respeitando as áreas de suas superfícies 3D.

---

## 7. Integração e consistência das ferramentas

Todas as novas funcionalidades devem compartilhar os sistemas existentes de seleção, transformação, histórico e renderização.

### Requisitos obrigatórios

- Undo/Redo para todas as operações.
- Atualização sincronizada entre viewport 3D e editor UV.
- Feedback visual para seleções e ferramentas ativas.
- Tratamento de erros e estados inválidos.
- Compatibilidade com o sistema de arquivos e formatos 3D já suportados.
- Preservação das coordenadas UV ao salvar e reabrir modelos.
- Operações eficientes em malhas de alta densidade.
- Evitar bloqueios prolongados da interface durante cálculos complexos.
- Preservar dados existentes, incluindo seams, UV sets e relações de ilhas.

### Testes

Criar testes para:

- Arraste individual e múltiplo de ilhas.
- Conversão entre coordenadas de tela e UV.
- Cálculo de caminhos de edges.
- Seleção em malhas desconectadas.
- Marcação e remoção de seams.
- Agrupamento e desagrupamento.
- Detecção e stacking de ilhas espelhadas.
- Preservação de stacks durante packing.
- Retificação de faixas curvas.
- Normalização de escala UV.
- Undo/Redo.
- Salvamento e carregamento.

Utilizar geometrias de teste conhecidas, como cubos, cilindros, arcos, faixas curvas e objetos simétricos.

---

## 8. Estratégia de implementação

Realize as modificações de forma incremental, na seguinte ordem:

**Fase 1 — Análise e arquitetura**

Inspecionar o código existente, identificar os componentes afetados e definir uma estratégia que minimize alterações desnecessárias.

**Fase 2 — Interface**

Aplicar a identidade visual e reorganizar a interface conforme as imagens de referência.

**Fase 3 — Interação**

Implementar movimentação de ilhas UV e seleção de caminhos de edges.

**Fase 4 — Manipulação UV**

Implementar agrupamento, espelhamento, stacking e retificação de ilhas.

**Fase 5 — Normalização**

Implementar Average Island Scale e normalização de texel density.

**Fase 6 — Integração**

Adaptar packing, histórico, persistência e demais sistemas afetados.

**Fase 7 — Validação**

Executar testes automatizados, corrigir regressões e validar os fluxos de trabalho.

### Diretrizes de execução

- Não reescrever a aplicação do zero.
- Não introduzir dependências sem necessidade justificada.
- Respeitar os padrões arquiteturais existentes.
- Separar algoritmos geométricos da camada de interface.
- Evitar duplicação de lógica.
- Preferir estruturas de dados reutilizáveis.
- Não implementar funcionalidades apenas visualmente: todos os controles devem executar operações reais.
- Não substituir implementações corretas por aproximações simplificadas.
- Não declarar uma funcionalidade concluída sem validá-la.
- Caso alguma funcionalidade dependa de uma mudança arquitetural relevante, explicar o motivo antes de realizá-la.

## 9. Entrega esperada

Ao finalizar, apresentar:

1. Resumo das funcionalidades implementadas.
2. Principais arquivos e módulos modificados.
3. Algoritmos e estruturas de dados utilizados.
4. Atalhos e interações disponíveis.
5. Testes executados e seus resultados.
6. Limitações conhecidas ou funcionalidades ainda pendentes.

**Resultado esperado:** transformar o UvCraft em um editor UV profissional, moderno e eficiente, com manipulação direta de ilhas, seleção inteligente de seams, stacking, retificação de UVs e normalização de escala, mantendo a estabilidade e a arquitetura da aplicação existente.

# Feature 10 — Integração Nativa Blender ↔ UvCraft

## Objetivo

Implementar uma integração bidirecional entre o Blender e o UvCraft, permitindo transferir modelos 3D, editar coordenadas UV e retornar os resultados ao Blender sem a necessidade de exportar e reimportar manualmente arquivos GLTF/GLB.

O UvCraft é desenvolvido em Rust e deve continuar funcionando como uma aplicação independente. A integração com Blender será opcional, distribuída junto com o UvCraft e implementada por meio de um addon Python utilizando a API oficial `bpy`.

A integração deve preservar a geometria original, materiais, modificadores, armatures, vertex groups, shape keys e animações do Blender, alterando apenas os dados explicitamente sincronizados.

---

## 10.1. Análise da arquitetura existente

Antes de implementar:

- Inspecionar a estrutura atual do projeto Rust.
- Identificar os módulos responsáveis por importação de modelos, representação de malhas, UVs, seams e persistência.
- Identificar o framework utilizado pela interface gráfica.
- Verificar como a aplicação gerencia documentos, objetos e atualizações de geometria.
- Identificar mecanismos existentes de Undo/Redo.
- Verificar se já existe alguma infraestrutura de comunicação entre processos.
- Identificar a estratégia atual de distribuição e empacotamento.

Não reestruturar o projeto desnecessariamente.

Criar a integração utilizando os padrões arquiteturais já adotados.

## 10.2. Estrutura da integração

Implementar três componentes independentes:

### A. UvCraft Bridge — Rust

Criar um módulo responsável pela comunicação com aplicações externas.

Responsabilidades:

- Inicializar e encerrar um servidor local.
- Receber modelos enviados pelo Blender.
- Validar os dados recebidos.
- Converter a representação recebida para as estruturas internas do UvCraft.
- Disponibilizar coordenadas UV e informações de seams atualizadas.
- Gerenciar sessões e identificar objetos sincronizados.
- Detectar alterações de topologia.
- Retornar mensagens de erro estruturadas.

Utilizar preferencialmente Axum com Tokio, caso sejam compatíveis com a arquitetura existente.

O servidor deve escutar exclusivamente em `127.0.0.1`, utilizando uma porta configurável.

Não expor o serviço na rede externa.

### B. UvCraft Blender Addon — Python

Desenvolver um addon utilizando `bpy`, compatível com as versões do Blender oficialmente suportadas pelo projeto.

O addon deve:

- Adicionar um painel UvCraft à interface do Blender.
- Identificar os objetos selecionados.
- Extrair geometria, topologia, coordenadas UV e seams.
- Enviar os dados ao UvCraft.
- Receber as alterações realizadas no UvCraft.
- Aplicar os dados UV à malha original.
- Exibir informações sobre o estado da conexão.
- Informar erros de sincronização.
- Suportar múltiplos objetos.

O addon não deve implementar algoritmos de unwrap, packing ou edição UV já disponíveis no UvCraft.

### C. Protocolo de comunicação

Definir um protocolo versionado entre o Blender e o UvCraft.

Na primeira versão, utilizar HTTP local com JSON, mantendo a serialização desacoplada para permitir formatos binários futuramente.

Implementar operações equivalentes a:

- Health Check.
- Send Mesh.
- Update Mesh.
- Get UV Data.
- Get Synchronization Status.
- Disconnect Session.

O protocolo deve permitir identificar a versão do addon, a versão da aplicação e a versão do próprio protocolo.

Implementar autenticação local por token de sessão, validação de origem quando aplicável e limites de tamanho das mensagens.

---

## 10.3. Painel UvCraft no Blender

Criar um painel na Sidebar do viewport 3D, acessível pela tecla `N`.

Nome do painel: **UvCraft**.

### Interface proposta

**Connection**

- Connection Status.
- Connect to UvCraft.
- Disconnect.

**Mesh Transfer**

- Send Selected Objects.
- Update Mesh.
- Receive UVs.

**Synchronization**

- Preserve Existing UVs.
- Validate Topology.
- Auto Sync (recurso futuro, inicialmente desativado).

**Information**

- Quantidade de objetos sincronizados.
- Nome do objeto ativo.
- Última sincronização.
- Estado da compatibilidade topológica.

Os comandos devem ser implementados como operadores reais do Blender, com tratamento de erros e feedback visual.

---

## 10.4. Envio de modelos do Blender para o UvCraft

Implementar a operação Send Mesh.

### Comportamento

1. O usuário seleciona um ou mais objetos do

# Feature 11 — UV Island Groups e Group Workspace

## Objetivo

Implementar no UvCraft um sistema de agrupamento de ilhas UV com seleção hierárquica, gerenciamento visual e transformações coletivas.

O usuário deve conseguir organizar várias ilhas em grupos persistentes, editar individualmente qualquer ilha e executar operações globais sobre um grupo, tratando-o como uma única unidade lógica.

O agrupamento não deve modificar a topologia, a geometria 3D ou a conectividade das ilhas.

## 11.1. Criação e gerenciamento de grupos

Permitir selecionar múltiplas ilhas UV e executar **Create Group**.

Cada grupo deve possuir:

- Identificador único e persistente.
- Nome editável.
- Lista de identificadores das ilhas pertencentes ao grupo.
- Estado de seleção.
- Estado de visibilidade.
- Estado de bloqueio opcional.
- Cor de identificação opcional.

Implementar os comandos:

- Create Group.
- Rename Group.
- Add Islands to Group.
- Remove Islands from Group.
- Ungroup.
- Select Group.
- Select Group Members.
- Delete Group.

Uma ilha deve pertencer a apenas um grupo comum por vez, nesta primeira implementação.

Ao excluir um grupo, suas ilhas devem permanecer no documento UV, preservando as coordenadas existentes.

Grupos não devem ser confundidos com stacks UV. O agrupamento representa uma relação organizacional e de transformação, enquanto o stacking representa sobreposição intencional de coordenadas UV.

## 11.2. Group Workspace — Faixa inferior do editor UV

Adicionar um painel horizontal, redimensionável e recolhível na parte inferior do viewport UV 2D.

O painel será chamado **Group Workspace**.

Quando houver um grupo ativo, o painel deve apresentar visualmente todas as ilhas pertencentes a ele.

### Interface

- Cabeçalho com nome do grupo ativo.
- Lista horizontal de miniaturas das ilhas.
- Identificação de cada ilha.
- Destaque visual da ilha selecionada.
- Indicador de quantidade de ilhas.
- Botão para selecionar o grupo inteiro.
- Botão para retornar à seleção individual.
- Menu contextual para remover ilhas do grupo.
- Navegação entre grupos existentes.

As miniaturas devem ser geradas a partir da geometria UV real, sem depender de imagens externas.

O painel deve suportar rolagem horizontal quando houver muitas ilhas.

Quando nenhum grupo estiver ativo, o painel poderá permanecer recolhido.

### Sincronização visual

Ao selecionar uma miniatura:

1. Identificar a ilha correspondente.
2. Selecionar essa ilha no editor UV.
3. Destacar sua geometria.
4. Opcionalmente centralizar a visualização sobre ela.
5. Permitir sua edição normal no viewport 2D.

A seleção deve funcionar nos dois sentidos: selecionar uma ilha no viewport também deve destacar sua miniatura quando ela pertencer ao grupo ativo.

O painel não deve substituir o editor UV principal. Ele funciona como um navegador de membros do grupo.

## 11.3. Sistema de seleção hierárquica

Implementar dois modos de seleção:

### Island Selection

Quando uma ilha individual estiver selecionada:

- Permitir mover.
- Permitir rotacionar.
- Permitir redimensionar.
- Permitir Relax.
- Permitir Straighten.
- Permitir recalcular sua escala.
- Permitir editar suas coordenadas UV normalmente.

As demais ilhas do grupo não devem ser alteradas.

### Group Selection

Quando o grupo estiver selecionado:

- Destacar todas as ilhas pertencentes ao grupo.
- Exibir um bounding box coletivo.
- Utilizar um pivô coletivo.
- Permitir mover, rotacionar e redimensionar o conjunto.
- Preservar as posições relativas entre as ilhas.
- Impedir que ferramentas globais tratem os membros como entidades independentes.

O modo de seleção ativo deve estar visualmente claro.

A seleção de uma ilha pela faixa inferior deve ativar Island Selection. A seleção do cabeçalho ou comando Select Group deve ativar Group Selection.

## 11.4. Transformações coletivas

Quando um grupo estiver selecionado, aplicar transformações sobre o conjunto inteiro.

### Move Group

Calcular uma única translação e aplicá-la a todos os vértices UV das ilhas pertencentes ao grupo.

### Rotate Group

Calcular um pivô coletivo e rotacionar todas as coordenadas UV ao redor dele.

### Scale Group

Aplicar um único fator de escala ao conjunto, utilizando o pivô coletivo.

O redimensionamento deve preservar:

- Proporções internas de cada ilha.
- Distâncias relativas entre as ilhas.
- Orientação relativa.
- Distribuição espacial do conjunto.

Utilizar escala uniforme como comportamento padrão.

A escala não uniforme poderá ser disponibilizada como operação explícita, caso já exista suporte adequado.

## 11.5. Group Packing

Modificar o sistema de packing para reconhecer grupos UV.

Quando um grupo participar do packing, ele deve ser tratado como uma unidade indivisível.

### Requisitos

- Não reorganizar individualmente os membros do grupo.
- Não modificar suas posições relativas.
- Não alterar suas escalas individuais.
- Permitir translação e rotação do grupo inteiro.
- Permitir escala uniforme do grupo, caso a configuração de packing autorize redimensionamento.
- Detectar colisões utilizando a geometria ocupada pelo conjunto.
- Preservar stacks intencionais existentes.

### Estratégia geométrica

Calcular uma representação da ocupação espacial do grupo.

Utilizar inicialmente uma bounding box coletiva, caso o algoritmo de packing existente dependa desse tipo de representação.

Quando possível, utilizar o contorno combinado das ilhas para melhorar o aproveitamento do espaço.

O algoritmo deve tratar grupos e ilhas independentes como unidades de packing distintas.

Se um grupo possuir três ilhas, essas três ilhas devem manter exatamente a mesma disposição relativa após o packing.

## 11.6. Recalcular escala proporcional do grupo

Integrar o sistema de grupos à funcionalidade Average Island Scale / Normalize Texel Density.

Quando uma ilha estiver selecionada individualmente, manter o comportamento original.

Quando um grupo estiver selecionado, aplicar um único fator de escala ao conjunto.

### Cálculo

Para o grupo, considerar:

- Soma das áreas 3D das faces de seus membros.
- Soma das áreas UV de seus membros.
- Densidade de textura agregada do conjunto.

Calcular um fator de escala uniforme para aproximar a densidade agregada do valor desejado.

Aplicar o mesmo fator a todos os membros, utilizando o pivô coletivo.

### Restrições

- Não normalizar individualmente as ilhas dentro do grupo.
- Não alterar a proporção relativa entre os membros.
- Não reorganizar o layout interno.
- Não modificar as distâncias relativas entre as ilhas, exceto pelo fator uniforme aplicado ao conjunto.
- Não desagrupar as ilhas.

**Importante:** a normalização coletiva pode preservar diferenças de texel density entre membros. O objetivo desta operação é ajustar a densidade agregada do grupo sem modificar sua organização interna.

Caso o usuário deseje uniformizar a densidade entre as ilhas, oferecer uma operação individual explícita.

Para grupos com stacks sobrepostos, distinguir área UV ocupada de área UV por superfície, evitando cálculos incorretos por sobreposição intencional.

## 11.7. Integração com ferramentas existentes

As ferramentas devem respeitar o contexto de seleção.

| Ferramenta | Ilha | Grupo |
|---|---|---|
| Move | Individual | Coletivo |
| Rotate | Individual | Coletivo |
| Scale | Individual | Coletivo |
| Pack | Conforme contexto | Unidade indivisível |
| Average Island Scale | Individual | Fator coletivo |
| Relax | Individual | Exigir operação explícita por membro |
| Straighten | Individual | Não executar automaticamente |
| Stack | Ilhas selecionadas | Operação explícita, sem confundir com Group |

Evitar implementar algoritmos duplicados.

Reutilizar as transformações UV existentes, adicionando uma camada de resolução de seleção que determine os elementos afetados.

## 11.8. Modelo de dados

Separar claramente:

- UV Island: geometria e coordenadas UV.
- UV Group: associação persistente entre ilhas.
- UV Stack: relação de sobreposição intencional.
- Selection Context: seleção de ilha ou grupo.
- Transform Operation: operação aplicada aos elementos selecionados.

Não duplicar coordenadas UV dentro dos grupos.

Os grupos devem referenciar as ilhas por identificadores estáveis.

Se uma operação de unwrap alterar a composição das ilhas, atualizar ou invalidar as associações afetadas de forma controlada.

Não reutilizar identificadores antigos quando uma ilha for dividida ou combinada sem uma correspondência confiável.

## 11.9. Persistência e Undo/Redo

O agrupamento deve ser preservado ao salvar e reabrir um projeto UvCraft.

Persistir:

- Identificadores.
- Nomes dos grupos.
- Relações entre grupos e ilhas.
- Propriedades organizacionais relevantes.

Não presumir que GLTF/GLB armazene nativamente os grupos do UvCraft.

Utilizar o formato de projeto da aplicação ou metadados específicos devidamente versionados.

Implementar Undo/Redo para:

- Criar grupo.
- Excluir grupo.
- Adicionar e remover ilhas.
- Renomear grupo.
- Mover grupo.
- Rotacionar grupo.
- Redimensionar grupo.
- Recalcular escala coletiva.
- Packing envolvendo grupos.

Cada operação interativa deve produzir uma entrada lógica no histórico, evitando registrar um comando separado por vértice ou por membro.

## 11.10. Integração com Blender

A integração Blender ↔ UvCraft deve continuar sincronizando as coordenadas UV reais das ilhas.

Os grupos UV serão inicialmente metadados internos do UvCraft.

Ao retornar UVs ao Blender:

- Aplicar normalmente as coordenadas resultantes.
- Não modificar a geometria.
- Não exigir que o Blender conheça o sistema de grupos.
- Preservar a estrutura dos grupos no projeto UvCraft.

O sistema de grupos não deve impedir a sincronização de UVs e seams.

## 11.11. Testes e critérios de aceitação

Criar testes para:

1. Criação de grupo com múltiplas ilhas.
2. Seleção de ilha pela faixa inferior.
3. Sincronização da seleção com o viewport.
4. Edição individual sem modificar outros membros.
5. Movimentação coletiva.
6. Rotação coletiva.
7. Escala coletiva preservando proporções.
8. Packing sem separar membros.
9. Recalcular escala usando um fator global.
10. Preservação de stacks dentro de grupos.
11. Exclusão de grupo sem excluir ilhas.
12. Undo/Redo de todas as operações.
13. Persistência ao salvar e reabrir.
14. Atualização segura de grupos após alterações de unwrap.
15. Sincronização com Blender sem perda de UVs.

### Critério principal

Dado um grupo formado por três ilhas UV com diferentes tamanhos e posições:

- Selecionar uma ilha permite editá-la individualmente.
- Selecionar o grupo permite transformar o conjunto inteiro.
- Redimensionar o grupo preserva a relação entre seus membros.
- Recalcular a escala do grupo utiliza um único fator coletivo.
- Executar Pack movimenta o conjunto como uma unidade, sem reorganizar as ilhas internamente.
- Salvar e reabrir o projeto preserva o agrupamento.

## Diretrizes de implementação

Inspecionar os sistemas existentes antes de modificar o código.

Priorizar reutilização de algoritmos, separação de responsabilidades, estabilidade e desempenho.

Implementar a faixa inferior como componente funcional da interface, não como protótipo estático.

Não confundir agrupamento com stacking.

Não permitir que operações coletivas alterem silenciosamente a organização interna do grupo.

Documentar os atalhos, comandos e limitações conhecidos.

Ao finalizar, apresentar os arquivos modificados, a arquitetura adotada, os testes executados e os resultados.

# Feature 12 — Refinamento do Stack Islands Existente

## Contexto

O UvCraft já possui uma funcionalidade chamada Stack Islands, responsável por sobrepor ilhas UV.

Entretanto, o algoritmo atual apresenta limitações ao tentar empilhar ilhas que possuem contornos geometricamente semelhantes, mas diferenças na triangulação, quantidade de vértices ou organização interna das faces.

Uma imagem de referência foi fornecida na pasta do projeto, mostrando duas ilhas UV com contornos aparentemente equivalentes, mas triangulações internas distintas.

Essas ilhas devem ser utilizadas como caso de teste principal.

**Objetivo:** refatorar e aprimorar o Stack Islands existente, sem criar uma segunda ferramenta com a mesma responsabilidade.

---

## 12.1. Investigar o comportamento atual

Antes de modificar o código, localizar a implementação existente de Stack Islands e analisar:

- Como as ilhas são identificadas.
- Como a equivalência geométrica é determinada.
- Se a comparação depende da quantidade de vértices.
- Se depende da quantidade de faces ou triângulos.
- Se exige correspondência entre índices.
- Se compara somente bounding boxes.
- Como calcula translação, rotação e espelhamento.
- Quais tolerâncias numéricas utiliza.
- Por que rejeita as ilhas mostradas na referência.

Criar um teste de regressão que reproduza a falha antes de implementar a correção.

Não substituir o algoritmo sem compreender suas limitações.

---

## 12.2. Separar contorno externo de topologia interna

A equivalência entre ilhas não deve depender obrigatoriamente de uma triangulação interna idêntica.

Implementar uma representação geométrica do contorno UV de cada ilha.

### Extração de contornos

Para cada ilha:

1. Identificar suas faces UV.
2. Identificar edges pertencentes à fronteira UV.
3. Reconstruir os loops de contorno.
4. Diferenciar contorno externo de eventuais contornos internos.
5. Remover apenas redundâncias geométricas seguras.
6. Preservar informações necessárias para mapear os segmentos aos elementos originais.

Edges internas não devem interferir na comparação de silhuetas.

Vértices colineares adicionais não devem impedir que duas ilhas sejam reconhecidas como equivalentes.

### Exemplo

Uma ilha pode possuir um contorno formado por oito segmentos, enquanto outra representa a mesma silhueta utilizando doze segmentos.

Se os contornos descreverem a mesma região geométrica dentro da tolerância definida, o sistema deve reconhecê-los como compatíveis para alinhamento por contorno.

Não exigir que ambas possuam a mesma quantidade de triângulos.

---

## 12.3. Alinhamento independente de orientação

O Stack Islands deve funcionar quando duas ilhas equivalentes estiverem:

- Transladadas.
- Rotacionadas em qualquer ângulo.
- Espelhadas.
- Com escala uniforme diferente.
- Com pontos iniciais de contorno diferentes.
- Com sentido de percurso invertido.

Implementar um alinhamento geométrico robusto, independente da ordenação original dos vértices.

### Estratégia

Utilizar uma abordagem baseada em correspondência de contornos.

1. Extrair os contornos das ilhas.
2. Normalizar temporariamente posição e escala para comparação.
3. Identificar possíveis correspondências entre pontos e segmentos.
4. Considerar diferentes rotações e reflexões.
5. Calcular a transformação de similaridade que melhor alinha os contornos.
6. Avaliar o erro residual.
7. Selecionar a solução geometricamente mais consistente.

Para contornos com correspondências conhecidas, utilizar Procrustes ou outro método equivalente.

Para contornos com quantidades diferentes de vértices, utilizar correspondência geométrica por segmentos ou amostragem de contorno, sem assumir correspondência direta entre índices.

Não depender exclusivamente de ICP sem restrições, pois ele pode convergir para alinhamentos incorretos em formas simétricas.

---

## 12.4. Validação geométrica

Após calcular o alinhamento, avaliar a qualidade da sobreposição.

Utilizar métricas como:

- Distância máxima entre contornos.
- Distância média entre contornos.
- Distância de Hausdorff, quando apropriada.
- Área da diferença simétrica entre as regiões UV.
- Porcentagem de sobreposição das áreas.
- Diferença relativa de escala.

Considerar todos os contornos relevantes, incluindo buracos quando existirem.

A comparação deve utilizar tolerâncias relativas ao tamanho da ilha.

Não classificar duas ilhas como equivalentes apenas porque seus bounding boxes são semelhantes.

### Classificação

**Exact Boundary Match**

Os contornos podem coincidir dentro da tolerância geométrica.

**Approximate Boundary Match**

Os contornos são semelhantes, mas apresentam diferenças residuais.

**Incompatible**

As regiões UV não podem ser alinhadas adequadamente utilizando uma transformação de similaridade.

---

## 12.5. Modos de empilhamento

Refinar a ferramenta existente para suportar dois comportamentos.

### A. Preserve Internal UVs

Modo padrão e não destrutivo.

- Alinhar a ilha utilizando translação, rotação, reflexão e escala uniforme, quando autorizada.
- Preservar sua parametrização interna.
- Preservar a triangulação.
- Não alterar a conectividade.
- Não deformar a ilha.
- Medir e informar qualquer diferença residual entre contornos.

Esse modo é indicado para ilhas geometricamente equivalentes.

### B. Conform to Reference

Modo opcional de conformação.

Quando duas ilhas possuem contornos compatíveis, mas parametrizações internas diferentes:

1. Selecionar uma ilha de referência.
2. Determinar a correspondência geométrica entre os contornos.
3. Alinhar os contornos da ilha de origem.
4. Ajustar as coordenadas UV dos vértices de fronteira para coincidir com o contorno de referência.
5. Recalcular as coordenadas UV internas utilizando um método adequado de parametrização com fronteira fixa.
6. Preservar a topologia original.
7. Validar orientação, distorção e integridade das faces.

O algoritmo pode utilizar métodos como parametrização harmônica ou relaxamento com fronteira fixa, desde que adequados à geometria.

Não aplicar deformações arbitrárias sem validação.

Se a conformação produzir faces invertidas, degeneradas ou distorção acima do limite configurado, cancelar a operação e informar o motivo.

**Importante:** contornos coincidentes não garantem correspondência de todos os pontos internos. Esse modo deve garantir a coincidência da fronteira dentro da tolerância, sem prometer equivalência de parametrização interna quando as topologias forem diferentes.

---

## 12.6. Ilha de referência

Permitir definir explicitamente qual ilha será utilizada como referência.

A ilha de referência deve permanecer imóvel e sem alterações nas coordenadas UV.

As demais ilhas serão alinhadas em relação a ela.

Quando nenhuma referência explícita for definida, utilizar uma regra previsível baseada na seleção, como a ilha ativa.

Informar visualmente qual ilha está sendo utilizada como referência.

---

## 12.7. Pré-visualização e diagnóstico

Antes de confirmar o stacking, permitir visualizar o resultado.

Exibir:

- Ilha de referência.
- Ilha sendo alinhada.
- Contornos sobrepostos.
- Diferenças residuais.
- Indicação de espelhamento.
- Escala aplicada.
- Qualidade da correspondência.

Quando a operação falhar, apresentar uma mensagem explicativa.

Exemplos:

- Contornos incompatíveis.
- Correspondência ambígua.
- Diferença geométrica acima da tolerância.
- Falha na parametrização interna.
- Faces UV invertidas após conformação.

Não utilizar apenas mensagens genéricas como "Stack failed".

---

## 12.8. Integração com sistema de grupos

Manter compatibilidade com UV Island Groups.

Uma ilha pode participar de um grupo e também de um stack.

Quando uma ilha pertencente a um grupo for empilhada:

- Preservar sua associação ao grupo.
- Atualizar suas coordenadas UV.
- Atualizar os limites espaciais do grupo.
- Atualizar a visualização no Group Workspace.
- Registrar a relação de stacking.

Não confundir Group com Stack.

Se uma transformação individual posterior quebrar a sobreposição, atualizar ou invalidar o estado do stack de maneira explícita.

---

## 12.9. Integração com Packing

O packing deve reconhecer stacks intencionais.

- Tratar um stack válido como uma unidade de ocupação UV.
- Preservar a sobreposição durante transformações coletivas.
- Não separar automaticamente seus membros.
- Não considerar overlaps intencionais como colisões indesejadas.
- Validar se a relação de stacking continua geometricamente válida.

Stacks com diferenças residuais devem utilizar a união das regiões ocupadas para detecção de colisões, evitando ignorar áreas não coincidentes.

---

## 12.10. Testes obrigatórios

Criar testes automatizados para:

1. Ilhas com contornos e triangulações idênticos.
2. Ilhas com contornos idênticos e triangulações diferentes.
3. Ilhas com vértices colineares adicionais.
4. Ilhas com diferentes quantidades de vértices internos.
5. Ilhas rotacionadas.
6. Ilhas espelhadas.
7. Ilhas rotacionadas e espelhadas simultaneamente.
8. Ilhas com escalas diferentes.
9. Ilhas com pontos iniciais de contorno diferentes.
10. Ilhas com sentidos de percurso invertidos.
11. Ilhas com contornos semelhantes, mas não equivalentes.
12. Ilhas com buracos.
13. Ilhas com simetria e múltiplas correspondências.
14. Conformação com triangulações diferentes.
15. Detecção de faces invertidas.
16. Undo/Redo.
17. Preservação de stacks durante packing.
18. Integração com UV Groups.

### Caso de regressão obrigatório

Reproduzir as duas ilhas apresentadas na imagem de referência.

Verificar que:

- Seus contornos sejam extraídos corretamente.
- As triangulações internas não impeçam a comparação de contornos.
- O algoritmo determine se as regiões são geometricamente equivalentes.
- Se equivalentes, permita alinhamento e stacking.
- Se houver diferenças geométricas, informe o erro residual.
- A operação preserve a geometria 3D.
- A sobreposição permaneça válida após salvar e reabrir o projeto.

---

## 12.11. Critérios de aceitação

O refinamento será considerado concluído quando:

- O Stack Islands existente continuar funcionando nos casos anteriores.
- Ilhas com contornos equivalentes e triangulações diferentes puderem ser alinhadas.
- Rotações e espelhamentos forem reconhecidos automaticamente.
- A ferramenta não depender de índices idênticos de vértices.
- O alinhamento por contorno não exigir topologias internas iguais.
- O modo Preserve Internal UVs não deformar as ilhas.
- O modo Conform to Reference conseguir ajustar casos compatíveis sem introduzir faces inválidas.
- O sistema informar claramente quando não for possível obter uma sobreposição válida.
- Undo/Redo e persistência continuarem funcionando.
- O packing preservar stacks intencionais.

## Diretriz final

Não criar uma nova ferramenta de stacking.

Refatorar a implementação existente, separando:

1. Extração de contornos.
2. Detecção de equivalência.
3. Cálculo de alinhamento.
4. Validação geométrica.
5. Aplicação da transformação.
6. Conformação opcional.
7. Registro e manutenção do stack.

Priorizar a solução menos destrutiva.

Não modificar a geometria 3D para forçar o alinhamento UV.

Utilizar a imagem fornecida como referência principal para reproduzir e corrigir a falha existente.