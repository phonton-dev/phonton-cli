import { useMemo, useState } from 'react'
import './App.css'
import { createInitialGame, legalMovesFor, movePiece } from './chessRules'
import type { GameState, Piece } from './chessRules'

const files = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'] as const
const ranks = ['8', '7', '6', '5', '4', '3', '2', '1'] as const
const namedPieces = ['king', 'queen', 'rook', 'bishop', 'knight', 'pawn'] as const

function squareName(file: string, rank: string) {
  return `${file}${rank}`
}

function pieceClass(piece: Piece | null) {
  return piece ? `piece piece-${piece.color}` : 'piece'
}

function App() {
  const [game, setGame] = useState<GameState>(() => createInitialGame())
  const [selected, setSelected] = useState<string | null>(null)
  const [notice, setNotice] = useState('White to move')

  const legalMoves = useMemo(
    () => (selected ? legalMovesFor(game, selected) : []),
    [game, selected],
  )

  const resetGame = () => {
    setGame(createInitialGame())
    setSelected(null)
    setNotice('White to move')
  }

  const selectSquare = (square: string) => {
    const piece = game.board[square] ?? null

    if (selected && legalMoves.includes(square)) {
      const result = movePiece(game, selected, square)
      if (result.ok) {
        setGame(result.state)
        setSelected(null)
        setNotice(result.move.capture ? `${result.move.notation} capture` : result.move.notation)
      } else {
        setNotice(result.error)
      }
      return
    }

    if (piece?.color === game.turn) {
      setSelected(square)
      setNotice(`${piece.name} on ${square}`)
      return
    }

    if (selected) {
      setNotice(`Illegal move from ${selected} to ${square}`)
      return
    }

    setNotice(`Select a ${game.turn} piece`)
  }

  return (
    <main className="app-shell">
      <section className="game-panel" aria-label="Chess board">
        <div className="status-row">
          <div>
            <p className="eyebrow">Local two-player chess</p>
            <h1>Chess</h1>
          </div>
          <button className="reset-button" type="button" onClick={resetGame}>
            Reset
          </button>
        </div>

        <div className="board" role="grid" aria-label="Playable chess board">
          {ranks.map((rank) =>
            files.map((file) => {
              const square = squareName(file, rank)
              const piece = game.board[square] ?? null
              const selectedSquare = selected === square
              const legalSquare = legalMoves.includes(square)
              const dark = (files.indexOf(file) + ranks.indexOf(rank)) % 2 === 1

              return (
                <button
                  aria-label={`${square}${piece ? ` ${piece.name}` : ''}`}
                  className={[
                    'square',
                    dark ? 'dark' : 'light',
                    selectedSquare ? 'selected' : '',
                    legalSquare ? 'legal' : '',
                  ].join(' ')}
                  key={square}
                  onClick={() => selectSquare(square)}
                  role="gridcell"
                  type="button"
                >
                  <span className="coord">{square}</span>
                  <span className={pieceClass(piece)}>{piece?.symbol ?? ''}</span>
                </button>
              )
            }),
          )}
        </div>
      </section>

      <aside className="side-panel" aria-label="Game status">
        <div className="status-card">
          <p className="eyebrow">Turn</p>
          <strong>{game.status.message}</strong>
          <span>{notice}</span>
        </div>
        <div className="piece-legend" aria-label="Named chess pieces">
          <p className="eyebrow">Pieces</p>
          <ul>
            {namedPieces.map((pieceName) => (
              <li key={pieceName}>{pieceName}</li>
            ))}
          </ul>
        </div>
        <div className="history-card">
          <p className="eyebrow">Moves</p>
          {game.history.length === 0 ? (
            <p>No moves yet.</p>
          ) : (
            <ol>
              {game.history.map((move, index) => (
                <li key={`${move.from}-${move.to}-${index}`}>
                  {index + 1}. {move.notation}
                </li>
              ))}
            </ol>
          )}
        </div>
      </aside>
    </main>
  )
}

export default App
