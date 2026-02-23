import { useState } from 'react'
import { Button } from '@/components/ui/button'

function App() {
    const [count, setCount] = useState(0)

    return (
        <div className="flex flex-col items-center justify-center min-h-screen">
            <h1 className="text-3xl font-bold underline mb-4">
                Vite + React + Tailwind + shadcn/ui
            </h1>
            <div className="card">
                <Button onClick={() => setCount((count) => count + 1)}>
                    count is {count}
                </Button>
                <p className="mt-4">
                    Edit <code>src/App.tsx</code> to test HMR
                </p>
            </div>
        </div>
    )
}

export default App
